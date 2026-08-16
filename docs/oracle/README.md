# Dialect oracle snippets

Locked header / table snippets used by `crates/protocol` dialect tests.
The full wire write-up is `../replica.md`.

| File | Why |
|---|---|
| `paths.h` | URL paths |
| `clients.c` / `clients.h` | quirk table (order matters) |
| `scanner.h` | object IDs `64` / `1` / `2` |
| `httpersist.h` | Keep-Alive rule |
| `upnpsoap.h` | DIDL namespaces |
| `w3c_date.c` | date normalize |

These files are third-party C excerpts kept for literal matching.
Do not treat them as the process model to port.
