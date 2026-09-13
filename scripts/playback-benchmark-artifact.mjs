// Observe cache publication and producer termination in a consistent order.
import { readdir, stat } from "node:fs/promises";
import { join } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";

export async function completedArtifact(cache, item) {
  const names = await readdir(cache).catch(() => []);
  for (const name of names) {
    if (!name.startsWith(`${item.id}-web-`) || !name.endsWith(".mp4")) continue;
    const path = join(cache, name);
    const [media, stamp, part] = await Promise.all([
      stat(path).catch(() => null), stat(`${path}.src`).catch(() => null), stat(`${path}.part`).catch(() => null),
    ]);
    if (media?.isFile() && media.size > 0 && stamp?.isFile() && stamp.size > 0 && !part) return path;
  }
  return null;
}

export async function waitForCompletedArtifact(cache, item, serverStatus, producers) {
  for (let attempt = 0; attempt < 1200; attempt++) {
    const artifact = await completedArtifact(cache, item);
    if (artifact) return artifact;
    const status = await serverStatus();
    if (attempt > 5 && status.transcode?.active === 0 && !(await producers(item)).length) {
      // Publication can finish after the directory read above and before these
      // producer observations. Inspect its files after observing termination;
      // the earlier absence cannot certify a failed publication.
      const published = await completedArtifact(cache, item);
      if (published) return published;
      throw new Error("Producer finished without a completed output and validation stamp");
    }
    await sleep(100);
  }
  throw new Error("Completed and stamped cache deadline");
}
