# MiniDLNA oracle snippets

Copied from the sibling tree `../minidlna` so this workspace can be
moved without losing the C literals. The full wire write-up is
`../replica.md`.

| File | Why |
|---|---|
| `minidlnapath.h` | URL paths |
| `clients.c` / `clients.h` | quirk table (order matters) |
| `scanner.h` | object IDs `64` / `1` / `2` |
| `httpersist.h` | Keep-Alive rule |
| `upnpsoap.h` | DIDL namespaces |
| `w3c_date.c` | date normalize (excerpt of `utils.c`) |

Do not treat these as the process model to port.
