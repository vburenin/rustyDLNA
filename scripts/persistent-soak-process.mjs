// Bounded process-group lifetime for fixture/evidence tools owned by the soak.
import { spawn } from "node:child_process";

export async function runCommand(program, args, { signal, timeout = 120000, outputLimit = 16 * 1024 * 1024, killGrace = 500 } = {}) {
  signal?.throwIfAborted();
  return new Promise((done, reject) => {
    const child = spawn(program, args, { detached: true, stdio: ["ignore", "pipe", "pipe"] });
    const chunks = [], diagnostics = [];
    let bytes = 0, diagnosticBytes = 0, failure, escalation;
    const signalGroup = (value) => {
      if (!child.pid) return;
      try { process.kill(-child.pid, value); } catch (error) { if (error.code !== "ESRCH") failure ||= error; }
    };
    const stop = (error) => {
      if (failure) return;
      failure = error; signalGroup("SIGTERM");
      escalation = setTimeout(() => signalGroup("SIGKILL"), killGrace);
    };
    const abort = () => stop(signal.reason);
    signal?.addEventListener("abort", abort, { once: true });
    const deadline = setTimeout(() => stop(new Error(`${program} exceeded ${timeout}ms helper deadline`)), timeout);
    child.stdout.on("data", (part) => {
      bytes += part.length;
      if (bytes > outputLimit) stop(new Error(`${program} exceeded output bound`));
      else chunks.push(part);
    });
    child.stderr.on("data", (part) => {
      diagnostics.push(part); diagnosticBytes += part.length;
      while (diagnosticBytes > 65536) diagnosticBytes -= diagnostics.shift().length;
    });
    child.once("error", (error) => { failure ||= error; });
    child.once("close", (code, value) => {
      clearTimeout(deadline); clearTimeout(escalation);
      signal?.removeEventListener("abort", abort);
      if (failure || code !== 0) reject(failure || new Error(`${program} failed (${code}/${value}): ${Buffer.concat(diagnostics).toString()}`));
      else done(Buffer.concat(chunks).toString().trim());
    });
  });
}
