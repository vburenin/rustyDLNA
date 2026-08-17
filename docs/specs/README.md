# Specifications

This directory holds the **public UPnP Forum** documents rustyDLNA is built
on. They were downloaded unmodified from `https://upnp.org/specs/`.

The document people mean by “the DLNA specification” is **IEC 62481**
(*Digital living network alliance (DLNA) home networked device
interoperability guidelines*). That series is sold by IEC/ISO and is
**not redistributable**. It is not in this tree. rustyDLNA advertises
`DLNADOC/1.50` (DLNA Guidelines 1.5) and implements a Digital Media
Server (DMS) subset plus the MiniDLNA wire dialect in
[`replica.md`](../../replica.md).

## What DLNA is vs what is here

```
IEC 62481  (DLNA Guidelines — member / paid, not here)
    └── requires
UPnP Device Architecture  (SSDP, HTTP, SOAP, GENA)     ← PDFs here
    └── MediaServer:1
            ├── ContentDirectory:1                     ← PDFs here
            ├── ConnectionManager:1                    ← PDFs here
            └── AVTransport:1 (optional; rustyDLNA does not expose it)
```

UPnP is also published as **ISO/IEC 29341**. Those ISO texts are the
same Forum DCPs with an ISO cover. Buy or view them from ISO if you
need the ISO edition.

## Official DLNA series (not vendored)

| Standard | Scope |
|---|---|
| IEC 62481-1 / 62481-1-1 | Architecture and protocols (DMS, DMP, DMR, …) |
| IEC 62481-2 | Media Format Profiles (`DLNA.ORG_PN`, JPEG_TN, AVC_*, …) |
| IEC 62481-3 | Link protection |
| IEC 62481-4 | DRM interoperability |
| IEC 62481-5 | Device profiles |
| IEC 62481-6 | Remote UI |
| IEC 62481-7 | Electronic program guide |
| IEC 62481-8 | Diagnostics |
| IEC 62481-9 | HTTP adaptive delivery |
| IEC 62481-10 | Low-power mode |

Purchase: [IEC webstore](https://webstore.iec.ch/) (search `62481`).
Alliance site: [dlna.org](https://www.dlna.org/).

Do not drop a leaked Guidelines PDF into this repo.

## Files in this directory

Normative for rustyDLNA (what `/rootDesc.xml` and SSDP advertise):

| File | Role in this tree |
|---|---|
| [`UPnP-arch-DeviceArchitecture-v1.0-20060720.pdf`](UPnP-arch-DeviceArchitecture-v1.0-20060720.pdf) | UDA 1.0: SSDP, description, SOAP control, GENA. Matches `UPnP/1.0` in `Server:`. |
| [`UPnP-av-AVArchitecture-v1.pdf`](UPnP-av-AVArchitecture-v1.pdf) | How MediaServer / MediaRenderer / Control Point talk. |
| [`UPnP-av-MediaServer-v1-Device.pdf`](UPnP-av-MediaServer-v1-Device.pdf) | Device type `MediaServer:1`; required CDS + CMS. |
| [`UPnP-av-ContentDirectory-v1-Service.pdf`](UPnP-av-ContentDirectory-v1-Service.pdf) | Browse, Search, DIDL-Lite, Filter, Sort, UpdateObject, 701/709/402. |
| [`UPnP-av-ConnectionManager-v1-Service.pdf`](UPnP-av-ConnectionManager-v1-Service.pdf) | `GetProtocolInfo` / connection stubs. |

Reference (later CDS / architecture; useful, not advertised):

| File | Why keep it |
|---|---|
| [`UPnP-arch-DeviceArchitecture-v1.1.pdf`](UPnP-arch-DeviceArchitecture-v1.1.pdf) | Later wording for SSDP / GENA. |
| [`UPnP-av-AVArchitecture-v2-20101231.pdf`](UPnP-av-AVArchitecture-v2-20101231.pdf) | AV Architecture:2. |
| [`UPnP-av-ContentDirectory-v3-Service.pdf`](UPnP-av-ContentDirectory-v3-Service.pdf) | SearchCriteria / SortCriteria expansions. |
| [`UPnP-av-ContentDirectory-v4-Service-20101231.pdf`](UPnP-av-ContentDirectory-v4-Service-20101231.pdf) | CDS:4 (`lastPlaybackPosition`, playCount). |
| [`UPnP-av-ConnectionManager-v2-Service.pdf`](UPnP-av-ConnectionManager-v2-Service.pdf) | CMS:2. |
| [`UPnP-av-AVTransport-v1-Service.pdf`](UPnP-av-AVTransport-v1-Service.pdf) | Play/seek on a **renderer**. Not a rustyDLNA service. |

`X_MS_MediaReceiverRegistrar` is a Microsoft vendor service, not a
UPnP Forum DCP. Behaviour is in `replica.md`.

## Map onto this tree

| Spec topic | rustyDLNA |
|---|---|
| SSDP alive / byebye / M-SEARCH | `crates/ssdp`, `replica.md` §1 |
| Device + SCPD XML | `crates/http` (`/rootDesc.xml`, `/ContentDir.xml`) |
| SOAP Browse / Search / faults | `crates/soap`, `crates/server` |
| DIDL-Lite + Filter | `crates/soap` |
| GENA SUBSCRIBE / NOTIFY | `crates/server/src/events.rs` |
| Object IDs, client flags, `DLNA.ORG_*` | `replica.md`, `crates/protocol` |
| Media GET, Range, `contentFeatures.dlna.org` | `crates/http`, `crates/server` |

When the Forum text and `replica.md` disagree, **`replica.md` wins**
for bytes on the wire. This project is a MiniDLNA-shaped DMS, not a
certified DLNA product.

## Re-fetch

```bash
./scripts/fetch-upnp-specs.sh
```

Checksums: [`SHA256SUMS`](SHA256SUMS).

## Copyright

© Contributing Members of the UPnP Forum. All rights reserved.
Unmodified copies of documents the Forum publishes at
[upnp.org/specs](https://upnp.org/specs/). Included only as offline
implementation reference. See each PDF cover for the Forum license
text. UPnP and DLNA are trademarks of their owners.
