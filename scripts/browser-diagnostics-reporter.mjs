import { appendFileSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";
import os from "node:os";

// One sampler per complete run, independent of browser worker scheduling.
export default class BrowserDiagnosticsReporter {
  onBegin(config, suite) {
    this.path = process.env.RUSTY_DLNA_BROWSER_EVIDENCE;
    mkdirSync(dirname(this.path), { recursive: true });
    writeFileSync(this.path, "");
    this.write({ kind: "run", workers: config.workers, tests: suite.allTests().length,
      node: process.version, platform: os.platform(), release: os.release(),
      cpus: os.cpus().length, cpuModel: os.cpus()[0]?.model, totalMemory: os.totalmem() });
    try { this.cgroup = `/sys/fs/cgroup${readFileSync("/proc/self/cgroup", "utf8").trim().split("::")[1]}`; }
    catch { this.cgroup = null; }
    this.sample();
    this.timer = setInterval(() => this.sample(), 1000);
    this.timer.unref();
  }
  write(value) { appendFileSync(this.path, `${JSON.stringify({ at: Date.now(), ...value })}\n`); }
  sample() {
    const files = {};
    for (const path of ["/proc/meminfo", "/proc/loadavg", "/proc/pressure/cpu", "/proc/pressure/memory", "/proc/pressure/io", "/proc/stat"]) {
      try { files[path] = readFileSync(path, "utf8").slice(0, 16384); }
      catch (error) { files[path] = { unavailable: error.code }; }
    }
    if (this.cgroup) for (const name of ["pids.current", "pids.max", "pids.events", "memory.current", "memory.max", "memory.events", "cpu.stat"]) {
      try { files[name] = readFileSync(`${this.cgroup}/${name}`, "utf8"); }
      catch (error) { files[name] = { unavailable: error.code }; }
    }
    let processTree, sockets;
    try {
      const rows = [];
      for (const pid of readdirSync("/proc").filter((name) => /^\d+$/.test(name))) {
        try {
          const raw = readFileSync(`/proc/${pid}/stat`, "utf8");
          const fields = raw.slice(raw.lastIndexOf(") ") + 2).split(" ");
          rows.push({ pid: Number(pid), parent: Number(fields[1]), threads: Number(fields[17]), rssPages: Number(fields[21]) });
        } catch { /* A process can exit between the directory and stat reads. */ }
      }
      const owned = new Set([process.pid]);
      let size;
      do { size = owned.size; for (const row of rows) if (owned.has(row.parent)) owned.add(row.pid); } while (size !== owned.size);
      const tree = rows.filter((row) => owned.has(row.pid));
      processTree = { processes: tree.length, threads: tree.reduce((n, row) => n + row.threads, 0),
        rssPages: tree.reduce((n, row) => n + row.rssPages, 0) };
      sockets = {};
      for (const line of readFileSync("/proc/net/tcp", "utf8").trim().split("\n").slice(1)) {
        const fields = line.trim().split(/\s+/);
        if (fields[1].endsWith(":4719")) sockets[fields[3]] = (sockets[fields[3]] || 0) + 1;
      }
    } catch (error) { processTree = { unavailable: error.code }; }
    this.write({ kind: "host", files, processTree, sockets });
  }
  onTestEnd(test, result) {
    const steps = [];
    let truncated = false;
    const limited = (items, limit) => {
      if (items.length > limit) truncated = true;
      return items.slice(0, limit);
    };
    const boundedText = (value, limit = 4000) => {
      if (value === undefined) return undefined;
      const text = String(value);
      if (text.length > limit) truncated = true;
      return text.slice(0, limit);
    };
    const visit = (items, depth = 0) => {
      for (const step of items) {
        if (steps.length >= 500 || depth >= 16) { truncated = true; return; }
        steps.push({ title: boundedText(step.title), duration: step.duration, error: boundedText(step.error?.message) });
        visit(step.steps, depth + 1);
      }
    };
    visit(result.steps);
    const errors = limited(result.errors, 16).map((error) => ({
      message: boundedText(error.message), value: boundedText(error.value), stack: boundedText(error.stack, 16000),
    }));
    const title = limited(test.titlePath(), 32).map((part) => boundedText(part));
    const annotations = limited(result.annotations, 64).map(({ type, description, location }) => ({
      type: boundedText(type), description: boundedText(description),
      ...(location ? { location: { file: boundedText(location.file), line: location.line, column: location.column } } : {}),
    }));
    const attachments = limited(result.attachments, 64).map(({ name, path, body }) => {
      const attachment = { name: boundedText(name), path: boundedText(path) };
      if (!body || !name.endsWith("evidence")) return attachment;
      const bytes = body.byteLength;
      if (bytes > 2 * 1024 * 1024) {
        truncated = true;
        return { ...attachment, evidenceStatus: "unavailable", evidenceUnavailable: "attachment-exceeds-byte-limit",
          evidenceBytes: bytes, evidenceByteLimit: 2 * 1024 * 1024 };
      }
      let evidenceTruncated = false;
      let remainingValues = 20000;
      // Valid JSON can still contain very deep trees or oversized strings. Keep
      // serialization bounded after parsing the byte-limited attachment.
      const evidenceValue = (value, depth = 0) => {
        if (--remainingValues < 0 || depth >= 32) {
          truncated = evidenceTruncated = true;
          return { unavailable: "diagnostic-value-limit" };
        }
        if (typeof value === "string") {
          if (value.length > 4000) truncated = evidenceTruncated = true;
          return value.slice(0, 4000);
        }
        if (Array.isArray(value)) {
          if (value.length > 2000) truncated = evidenceTruncated = true;
          return value.slice(0, 2000).map((item) => evidenceValue(item, depth + 1));
        }
        if (value && typeof value === "object") {
          const entries = Object.entries(value);
          if (entries.length > 128) truncated = evidenceTruncated = true;
          return Object.fromEntries(entries.slice(0, 128).map(([key, item]) => {
            if (key.length > 4000) truncated = evidenceTruncated = true;
            return [key.slice(0, 4000), evidenceValue(item, depth + 1)];
          }));
        }
        return value;
      };
      try {
        const evidence = evidenceValue(JSON.parse(body.toString()));
        return { ...attachment, evidenceStatus: evidenceTruncated ? "truncated" : "captured", evidence };
      } catch (error) {
        return { ...attachment, evidenceStatus: "unavailable", evidenceUnavailable: "invalid-json",
          evidenceError: boundedText(error.message) };
      }
    });
    // Build all bounded fields first so truncation discovered in an attachment,
    // annotation or title is reflected by the final record's flag.
    this.write({ kind: "test", title, status: result.status,
      duration: result.duration, retry: result.retry, worker: result.workerIndex,
      errors, annotations, steps, attachments, truncated,
      droppedAttachments: Math.max(0, result.attachments.length - attachments.length),
      droppedAnnotations: Math.max(0, result.annotations.length - annotations.length),
    });
  }
  onEnd(result) { clearInterval(this.timer); this.sample(); this.write({ kind: "result", ...result }); }
}
