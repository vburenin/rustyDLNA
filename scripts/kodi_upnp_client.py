#!/usr/bin/env python3
"""Kodi / Platinum UPnP client against a rustyDLNA listener.

Constants and request shape are copied from Kodi's tree
(https://github.com/xbmc/xbmc), not invented:

- UA: lib/libUPnP/Platinum/Source/Core/PltHttp.h
      + Platinum/Source/Platinum/PltVersion.h  (1.0.5.13)
- Browse Filter + page size 200:
      lib/libUPnP/Platinum/Source/Devices/MediaServer/PltSyncMediaBrowser.h:73
      PltSyncMediaBrowser.cpp:427  (metadata?1:200)
- M-SEARCH MAN quotes + MX=5:
      Platinum/Source/Core/PltCtrlPoint.cpp:399-403
- Date rules (1905 bug): Neptune/Source/Core/NptTime.cpp:470-492
- Resource pick: xbmc/network/upnp/UPnPInternal.cpp ResourcePrioritySort

This is a control-point, not a GUI Kodi. Full Kodi is not built here.
"""

from __future__ import annotations

import argparse
import html
import socket
import sys
import time
import xml.etree.ElementTree as ET
from urllib.parse import urljoin, urlparse

# PltHttp.h + PltVersion.h
PLT_UA = "UPnP/1.0 DLNADOC/1.50 Platinum/1.0.5.13"
# Kodi also sends this on some HTTP paths (UPnPInternal / FileItem).
KODI_UA = "Kodi/21.0 (Linux) " + PLT_UA

# PltSyncMediaBrowser.h PLT_DEFAULT_FILTER
PLT_DEFAULT_FILTER = (
    "dc:date,dc:description,upnp:longDescription,upnp:genre,res,res@duration,"
    "res@size,upnp:albumArtURI,upnp:rating,upnp:lastPlaybackPosition,"
    "upnp:lastPlaybackTime,upnp:playbackCount,upnp:originalTrackNumber,"
    "upnp:episodeNumber,upnp:programTitle,upnp:seriesTitle,upnp:album,"
    "upnp:artist,upnp:author,upnp:director,dc:publisher,searchable,childCount,"
    "dc:title,dc:creator,upnp:actor,res@resolution,upnp:episodeCount,"
    "upnp:episodeSeason,xbmc:lastPlayerState,xbmc:dateadded,xbmc:rating,"
    "xbmc:votes,xbmc:artwork,xbmc:uniqueidentifier,xbmc:country,xbmc:userrating"
)

NS = {
    "s": "http://schemas.xmlsoap.org/soap/envelope/",
    "u": "urn:schemas-upnp-org:service:ContentDirectory:1",
    "d": "urn:schemas-upnp-org:device-1-0",
    "didl": "urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/",
    "dc": "http://purl.org/dc/elements/1.1/",
    "upnp": "urn:schemas-upnp-org:metadata-1-0/upnp/",
}


class Fail(Exception):
    pass


def npt_datetime_w3c_ok(raw: str) -> bool:
    """NPT_DateTime::FromString FORMAT_W3C (NptTime.cpp:470-492)."""
    if not raw:
        return False
    n = len(raw)
    if n < 17 and n != 10:
        return False
    if raw[4] != "-" or raw[7] != "-":
        return False
    if n == 10:
        return True
    if raw[10] != "T" or raw[13] != ":":
        return False
    if n > 16 and raw[16] == ":":
        # seconds present → Platinum requires length >= 20 (…SSZ or offset)
        return n >= 20
    return True


def raw_http(host: str, port: int, req: str, timeout: float = 8.0) -> tuple[int, str, bytes]:
    s = socket.create_connection((host, port), timeout=timeout)
    try:
        s.sendall(req.encode("utf-8", "surrogateescape"))
        chunks = []
        while True:
            b = s.recv(65536)
            if not b:
                break
            chunks.append(b)
    finally:
        s.close()
    buf = b"".join(chunks)
    split = buf.find(b"\r\n\r\n")
    head = buf[: split if split >= 0 else len(buf)].decode("latin1", "replace")
    body = buf[split + 4 :] if split >= 0 else b""
    status = 0
    parts = head.split()
    if len(parts) >= 2:
        try:
            status = int(parts[1])
        except ValueError:
            status = 0
    return status, head, body


def soap(host: str, port: int, path: str, action: str, inner: str, ua: str = PLT_UA) -> str:
    envelope = (
        '<?xml version="1.0"?>'
        '<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" '
        's:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">'
        f"<s:Body>{inner}</s:Body></s:Envelope>"
    )
    req = (
        f"POST {path} HTTP/1.1\r\n"
        f"Host: {host}:{port}\r\n"
        f"User-Agent: {ua}\r\n"
        f'SOAPAction: "urn:schemas-upnp-org:service:ContentDirectory:1#{action}"\r\n'
        "Content-Type: text/xml; charset=\"utf-8\"\r\n"
        f"Content-Length: {len(envelope)}\r\n"
        "Connection: close\r\n\r\n"
        f"{envelope}"
    )
    st, hdr, body = raw_http(host, port, req)
    text = body.decode("utf-8", "replace")
    if st != 200:
        raise Fail(f"{action} HTTP {st} {hdr}\n{text[:800]}")
    return text


def browse(host: str, port: int, path: str, oid: str, start: int = 0, count: int = 200) -> str:
    inner = (
        '<u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">'
        f"<ObjectID>{html.escape(oid)}</ObjectID>"
        "<BrowseFlag>BrowseDirectChildren</BrowseFlag>"
        f"<Filter>{html.escape(PLT_DEFAULT_FILTER)}</Filter>"
        f"<StartingIndex>{start}</StartingIndex>"
        f"<RequestedCount>{count}</RequestedCount>"
        "<SortCriteria></SortCriteria>"
        "</u:Browse>"
    )
    return soap(host, port, path, "Browse", inner)


def unescape_didl(soap_xml: str) -> str:
    # SOAP Result contains escaped DIDL.
    try:
        root = ET.fromstring(soap_xml)
    except ET.ParseError as e:
        raise Fail(f"SOAP XML parse: {e}\n{soap_xml[:400]}") from e
    result = None
    for el in root.iter():
        if el.tag.endswith("Result") and el.text:
            result = el.text
            break
    if result is None:
        raise Fail(f"no <Result> in SOAP:\n{soap_xml[:600]}")
    return html.unescape(result)


def parse_objects(didl: str) -> list[dict]:
    root = ET.fromstring(didl)
    out = []
    for node in list(root):
        tag = node.tag.rsplit("}", 1)[-1]
        title = ""
        klass = ""
        date = ""
        res = []
        for ch in node:
            ln = ch.tag.rsplit("}", 1)[-1]
            if ln == "title":
                title = (ch.text or "").strip()
            elif ln == "class":
                klass = (ch.text or "").strip()
            elif ln == "date":
                date = (ch.text or "").strip()
            elif ln == "res" and ch.text:
                res.append(
                    {
                        "url": ch.text.strip(),
                        "protocol": ch.attrib.get("protocolInfo", ""),
                        "size": ch.attrib.get("size"),
                    }
                )
        out.append(
            {
                "tag": tag,
                "id": node.attrib.get("id", ""),
                "title": title,
                "class": klass,
                "date": date,
                "res": res,
            }
        )
    return out


def pick_resource(obj: dict) -> dict | None:
    """UPnPInternal.cpp ResourcePrioritySort: matching mime + http-get."""
    kind = "video"
    if "audioItem" in obj["class"]:
        kind = "audio"
    elif "imageItem" in obj["class"]:
        kind = "image"

    def prio(r: dict) -> int:
        p = 0
        proto = r["protocol"]
        if f"{kind}/" in proto:
            p += 400
        if proto.startswith("http-get"):
            p += 100
        return p

    if not obj["res"]:
        return None
    return max(obj["res"], key=prio)


def control_url(host: str, port: int) -> str:
    st, _, body = raw_http(
        host,
        port,
        f"GET /rootDesc.xml HTTP/1.1\r\nHost: {host}:{port}\r\nUser-Agent: {PLT_UA}\r\nConnection: close\r\n\r\n",
    )
    if st != 200:
        raise Fail(f"rootDesc HTTP {st}")
    xml = body.decode("utf-8", "replace")
    if "MediaServer:1" not in xml and "MediaServer:1" not in xml.replace(" ", ""):
        raise Fail("rootDesc missing MediaServer:1")
    root = ET.fromstring(xml)
    for sc in root.iter():
        if sc.tag.endswith("serviceType") and sc.text and "ContentDirectory" in sc.text:
            parent = None
            # walk siblings via parent map
            break
    # simpler scan
    text = xml
    idx = text.find("ContentDirectory")
    chunk = text[idx : idx + 800] if idx >= 0 else text
    start = chunk.find("<controlURL>")
    end = chunk.find("</controlURL>")
    if start >= 0 and end > start:
        return chunk[start + len("<controlURL>") : end].strip() or "/"
    return "/"


def msearch(ssdp_port: int, host: str = "127.0.0.1") -> str:
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.settimeout(2.0)
    pkt = (
        "M-SEARCH * HTTP/1.1\r\n"
        f"HOST: {host}:{ssdp_port}\r\n"
        'MAN: "ssdp:discover"\r\n'
        "MX: 5\r\n"
        "ST: upnp:rootdevice\r\n"
        f"USER-AGENT: {PLT_UA}\r\n"
        "\r\n"
    )
    sock.sendto(pkt.encode(), (host, ssdp_port))
    try:
        data, _ = sock.recvfrom(4096)
    except TimeoutError as e:
        raise Fail("Platinum M-SEARCH (quoted MAN, ST=upnp:rootdevice) got no reply") from e
    finally:
        sock.close()
    text = data.decode("latin1", "replace")
    if "200" not in text.split("\n", 1)[0] and "HTTP/1.1 200" not in text:
        raise Fail(f"bad M-SEARCH reply: {text[:200]}")
    if "/rootDesc.xml" not in text:
        raise Fail(f"M-SEARCH reply missing LOCATION rootDesc: {text[:300]}")
    return text


def walk(host: str, port: int) -> None:
    path = control_url(host, port)
    # Kodi opens upnp://uuid/ → Browse 0, then user walks Video / All Video.
    root = parse_objects(unescape_didl(browse(host, port, path, "0")))
    titles = {o["title"] for o in root}
    if not any(t in titles for t in ("Video", "Browse Folders", "Music")):
        raise Fail(f"Browse 0 missing expected containers: {titles}")

    video = parse_objects(unescape_didl(browse(host, port, path, "2")))
    vtitles = {o["title"] for o in video}
    if "All Video" not in vtitles:
        raise Fail(f"Browse 2 missing All Video: {vtitles}")

    items = parse_objects(unescape_didl(browse(host, port, path, "2$8")))
    movies = [o for o in items if "videoItem" in o["class"]]
    if not movies:
        raise Fail(f"Browse 2$8 no videoItem: {items}")

    fixture = next((o for o in movies if "Fixture Movie" in o["title"]), movies[0])
    if fixture["date"] and not npt_datetime_w3c_ok(fixture["date"]):
        raise Fail(
            f"Kodi NPT_DateTime would reject dc:date={fixture['date']!r} "
            f"(19-char datetime → 1905). title={fixture['title']!r}"
        )
    if fixture["date"] and len(fixture["date"]) == 19:
        raise Fail(f"19-char dc:date {fixture['date']!r} is the Kodi 1905 bug")

    res = pick_resource(fixture)
    if not res:
        raise Fail(f"no <res> on {fixture['title']}")
    if "http-get" not in res["protocol"]:
        raise Fail(f"Platinum would skip non-http-get res: {res}")
    url = res["url"]
    parsed = urlparse(url)
    # DIDL is escaped in SOAP; we already unescaped. URL may be http://ip:port/MediaItems/n.mkv
    media_host = parsed.hostname or host
    media_port = parsed.port or port
    media_path = parsed.path or url
    get = (
        f"GET {media_path} HTTP/1.1\r\n"
        f"Host: {media_host}:{media_port}\r\n"
        f"User-Agent: {PLT_UA}\r\n"
        "Connection: close\r\n\r\n"
    )
    st, hdr, body = raw_http(media_host, media_port, get)
    if st != 200:
        raise Fail(f"Platinum GET {media_path} → {st} {hdr}")
    if len(body) < 8:
        raise Fail(f"media body too small: {len(body)}")
    print(
        f"Kodi/Platinum walk OK: Browse 0/2/2$8, "
        f"item={fixture['title']!r} date={fixture['date']!r} "
        f"GET {media_path} {st} {len(body)} bytes"
    )


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--http", type=int, required=True)
    ap.add_argument("--ssdp", type=int, required=True)
    ap.add_argument("--host", default="127.0.0.1")
    args = ap.parse_args()
    try:
        msearch(args.ssdp, args.host)
        walk(args.host, args.http)
    except Fail as e:
        print(f"Kodi/Platinum client FAILED: {e}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
