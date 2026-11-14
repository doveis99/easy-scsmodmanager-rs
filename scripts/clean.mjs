// Trim the Cargo target folder, which grows to several GB.
//
//     pnpm clean          keep what rebuilding the app needs, drop the rest
//     pnpm clean --all    drop the whole target folder (next build is a full one)
//
// Kept by the default mode: the release build cache (target/release/deps,
// build, .fingerprint), so `pnpm portable` stays a matter of seconds.
// Dropped: the debug build that `cargo test` / `cargo check` produce (by far
// the largest part), installer leftovers, example binaries and docs. All of
// it is regenerated on demand - nothing here is source.

import { existsSync, readdirSync, rmSync, statSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const target = join(root, "target");
const all = process.argv.includes("--all");

function sizeOf(path) {
  let total = 0;
  const stack = [path];
  while (stack.length) {
    const current = stack.pop();
    let entries;
    try {
      entries = readdirSync(current, { withFileTypes: true });
    } catch {
      continue;
    }
    for (const entry of entries) {
      const full = join(current, entry.name);
      if (entry.isDirectory()) stack.push(full);
      else {
        try {
          total += statSync(full).size;
        } catch {}
      }
    }
  }
  return total;
}

const gb = (bytes) => `${(bytes / 1024 ** 3).toFixed(2)} GB`;

if (!existsSync(target)) {
  console.log("no target folder - nothing to clean");
  process.exit(0);
}

const before = sizeOf(target);
const doomed = all
  ? [target]
  : [
      join(target, "debug"),
      join(target, "doc"),
      join(target, "tmp"),
      join(target, "release", "bundle"),
      join(target, "release", "nsis"),
      join(target, "release", "examples"),
      join(target, "release", "incremental"),
    ];

for (const path of doomed) {
  if (!existsSync(path)) continue;
  const size = sizeOf(path);
  rmSync(path, { recursive: true, force: true });
  console.log(`removed ${path.slice(root.length + 1)}  (${gb(size)})`);
}

const after = existsSync(target) ? sizeOf(target) : 0;
console.log(`target: ${gb(before)} -> ${gb(after)}`);
