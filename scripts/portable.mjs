// Build the portable folder: one executable that runs where it sits and
// keeps all of its data in a `data` folder beside it.
//
//     pnpm portable
//
// Output: dist-portable/Easy SCS Mod Manager/  (+ a .zip of it on Windows)
// An existing `data` folder in the output is never touched, so rebuilding
// over a folder that is in use keeps its settings and backups.

import { execFileSync, execSync } from "node:child_process";
import { copyFileSync, existsSync, mkdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const windows = process.platform === "win32";
const exeName = windows ? "easy-scsmodmanager.exe" : "easy-scsmodmanager";
const version = /^version\s*=\s*"([^"]+)"/m.exec(readFileSync(join(root, "Cargo.toml"), "utf8"))?.[1] ?? "0.0.0";
const folderName = "Easy SCS Mod Manager";
const outRoot = join(root, "dist-portable");
const outDir = join(outRoot, folderName);

console.log(`building ${folderName} ${version} ...`);
execSync("pnpm tauri build --no-bundle", { cwd: root, stdio: "inherit" });

const built = join(root, "target", "release", exeName);
if (!existsSync(built)) {
  console.error(`build produced no ${built}`);
  process.exit(1);
}

mkdirSync(outDir, { recursive: true });
copyFileSync(built, join(outDir, exeName));
copyFileSync(join(root, "LICENSE"), join(outDir, "LICENSE.txt"));
writeFileSync(
  join(outDir, "README.txt"),
  [
    `Easy SCS Mod Manager ${version} (portable)`,
    "",
    `설치가 필요 없습니다. ${exeName} 를 실행하세요.`,
    "",
    "- 설정, 분류 고정, 즐겨찾기, 프로필 백업, 창작마당 캐시는 모두 이 폴더의",
    "  data 폴더에 저장됩니다. 폴더를 통째로 옮기면 그대로 따라갑니다.",
    "- data 폴더를 지우면 초기 상태로 돌아갑니다. 프로필 백업도 함께 지워집니다.",
    "- 폴더에 쓸 수 없는 위치(Program Files 등)에서 실행하면 사용자 폴더",
    "  (%APPDATA%\\easy-scsmodmanager-rs)를 대신 씁니다.",
    "- Microsoft Edge WebView2 런타임이 필요합니다. Windows 11에는 기본으로",
    "  들어 있습니다.",
    "",
    "GPL-3.0-or-later. Switch-Bros/easy-scsmodmanager 를 Rust로 다시 쓴 것입니다.",
    "",
  ].join("\r\n"),
);

const sizeMb = (statSync(join(outDir, exeName)).size / 1024 / 1024).toFixed(1);
console.log(`\nportable folder: ${outDir}  (${exeName}, ${sizeMb} MB)`);

if (windows) {
  // a zip for passing the app along - without the data folder
  const zip = join(outRoot, `Easy-SCS-Mod-Manager-${version}-portable.zip`);
  rmSync(zip, { force: true });
  const files = [exeName, "LICENSE.txt", "README.txt"].map((f) => `'${join(outDir, f).replaceAll("'", "''")}'`).join(",");
  execFileSync(
    "powershell.exe",
    ["-NoProfile", "-NonInteractive", "-Command", `Compress-Archive -Path ${files} -DestinationPath '${zip.replaceAll("'", "''")}'`],
    { stdio: "inherit" },
  );
  console.log(`zip:             ${zip}`);
}
