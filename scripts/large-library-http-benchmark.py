#!/usr/bin/env python3
"""Bounded SOAP and web-library latency driver used by the 50k benchmark."""

import argparse
import http.client
import json
import math
import os
import re
import statistics
import time


SERVICE = "urn:schemas-upnp-org:service:ContentDirectory:1"


def envelope(action: str, fields: dict[str, str]) -> bytes:
    args = "".join(f"<{name}>{value}</{name}>" for name, value in fields.items())
    return (
        '<?xml version="1.0"?>'
        '<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" '
        's:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">'
        f'<s:Body><u:{action} xmlns:u="{SERVICE}">{args}</u:{action}></s:Body>'
        "</s:Envelope>"
    ).encode()


def call(port: int, action: str, fields: dict[str, str]) -> tuple[bytes, float]:
    body = envelope(action, fields)
    started = time.perf_counter()
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=30)
    connection.request(
        "POST",
        "/ctl/ContentDir",
        body,
        {
            "Content-Type": 'text/xml; charset="utf-8"',
            "SOAPACTION": f'"{SERVICE}#{action}"',
            "Connection": "close",
        },
    )
    response = connection.getresponse()
    payload = response.read()
    connection.close()
    elapsed_ms = (time.perf_counter() - started) * 1_000
    if response.status != 200 or b"<NumberReturned>" not in payload:
        raise RuntimeError(
            f"{action} returned HTTP {response.status}: {payload[:500]!r}"
        )
    return payload, elapsed_ms


def web_call(port: int, query: str) -> tuple[dict, float]:
    started = time.perf_counter()
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=30)
    connection.request("GET", f"/api/web/library?{query}", headers={"Accept": "application/json", "Connection": "close"})
    response = connection.getresponse()
    payload = response.read()
    connection.close()
    elapsed_ms = (time.perf_counter() - started) * 1_000
    parsed = json.loads(payload)
    if response.status != 200 or parsed.get("schema_version") != 1:
        raise RuntimeError(f"web library returned HTTP {response.status}: {payload[:500]!r}")
    return parsed, elapsed_ms


def fields(action: str) -> dict[str, str]:
    common = {
        "Filter": "dc:title,upnp:class,res",
        "StartingIndex": "0",
        "RequestedCount": "64",
        "SortCriteria": "+dc:title",
    }
    if action == "Browse":
        return {
            "ObjectID": "2$8",
            "BrowseFlag": "BrowseDirectChildren",
            **common,
        }
    return {
        "ContainerID": "0",
        "SearchCriteria": 'upnp:class derivedfrom "object.item.videoItem"',
        **common,
    }


def percentile(values: list[float], quantile: float) -> float:
    ordered = sorted(values)
    index = max(0, math.ceil(len(ordered) * quantile) - 1)
    return ordered[index]


def benchmark(port: int, requests: int, warmups: int, web_p95_target_ms: float) -> None:
    output: dict[str, dict[str, float | int]] = {}
    for action in ("Browse", "Search"):
        request_fields = fields(action)
        for _ in range(warmups):
            call(port, action, request_fields)
        samples = [call(port, action, request_fields)[1] for _ in range(requests)]
        output[action.lower()] = {
            "requests": requests,
            "p50_ms": round(percentile(samples, 0.50), 3),
            "p95_ms": round(percentile(samples, 0.95), 3),
            "p99_ms": round(percentile(samples, 0.99), 3),
            "mean_ms": round(statistics.fmean(samples), 3),
            "max_ms": round(max(samples), 3),
        }
    web_queries = {
        "web_first_page": "view=library&kind=video&sort=title&offset=0&limit=64",
        "web_later_page": "view=library&kind=video&sort=title&offset=40000&limit=64",
        "web_search": "view=library&kind=video&sort=title&q=Title%204&offset=0&limit=64",
    }
    for name, query in web_queries.items():
        for _ in range(warmups):
            web_call(port, query)
        samples = [web_call(port, query)[1] for _ in range(requests)]
        p95 = percentile(samples, 0.95)
        output[name] = {
            "requests": requests,
            "p50_ms": round(percentile(samples, 0.50), 3),
            "p95_ms": round(p95, 3),
            "p99_ms": round(percentile(samples, 0.99), 3),
            "mean_ms": round(statistics.fmean(samples), 3),
            "max_ms": round(max(samples), 3),
            "p95_target_ms": web_p95_target_ms,
        }
        if p95 > web_p95_target_ms:
            raise RuntimeError(f"{name} p95 {p95:.3f}ms exceeded {web_p95_target_ms:.3f}ms target")
    print(json.dumps(output, sort_keys=True))


def wait_for_total(port: int, expected: int, timeout: float, create: str | None) -> None:
    started = time.perf_counter()
    if create:
        with open(create, "xb") as output:
            output.write(bytes([0x1A, 0x45, 0xDF, 0xA3]) + bytes(60))
            output.flush()
            os.fsync(output.fileno())
    total_pattern = re.compile(rb"<TotalMatches>(\d+)</TotalMatches>")
    attempts = 0
    while time.perf_counter() - started < timeout:
        attempts += 1
        payload, _ = call(port, "Browse", fields("Browse"))
        match = total_pattern.search(payload)
        if match and int(match.group(1)) >= expected:
            print(
                json.dumps(
                    {
                        "latency_ms": round((time.perf_counter() - started) * 1_000, 3),
                        "polls": attempts,
                        "total_matches": int(match.group(1)),
                    },
                    sort_keys=True,
                )
            )
            return
        time.sleep(0.05)
    raise TimeoutError(f"catalog did not reach {expected} objects in {timeout} seconds")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, required=True)
    subparsers = parser.add_subparsers(dest="command", required=True)
    latency = subparsers.add_parser("latency")
    latency.add_argument("--requests", type=int, default=200)
    latency.add_argument("--warmups", type=int, default=10)
    latency.add_argument("--web-p95-target-ms", type=float, default=250.0)
    update = subparsers.add_parser("wait-for-total")
    update.add_argument("--expected", type=int, required=True)
    update.add_argument("--timeout", type=float, default=120)
    update.add_argument("--create")
    args = parser.parse_args()
    if args.command == "latency":
        benchmark(args.port, args.requests, args.warmups, args.web_p95_target_ms)
    else:
        wait_for_total(args.port, args.expected, args.timeout, args.create)


if __name__ == "__main__":
    main()
