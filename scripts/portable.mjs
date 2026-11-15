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
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const windows = process.platform === "win32";
const exeName = windows ? "easy-scsmodmanager.exe" : "easy-scsmodmanager";
const version = /^version\s*=\s*"([^"]+)"/m.exec(readFileSync(join(root, "Cargo.toml"), "utf8"))?.[1] ?? "0.0.0";
const folderName = "Easy SCS Mod Manager";
const outRoot = join(root, "dist-portable");
const outDir = join(outRoot, folderName);

// The compiler bakes absolute source paths into the binary (panic
// messages of every dependency): the builder's home folder, and with it
// their user name, would ship in the executable. Map them away.
const flags = [
  ...(process.env.CARGO_ENCODED_RUSTFLAGS?.split("\x1f") ?? process.env.RUSTFLAGS?.split(/\s+/) ?? []).filter(Boolean),
  `--remap-path-prefix=${homedir()}=~`,
  `--remap-path-prefix=${root}=.`,
];

console.log(`building ${folderName} ${version} ...`);
execSync("pnpm tauri build --no-bundle", {
  cwd: root,
  stdio: "inherit",
  env: { ...process.env, CARGO_ENCODED_RUSTFLAGS: flags.join("\x1f") },
});

const built = join(root, "target", "release", exeName);
if (!existsSync(built)) {
  console.error(`build produced no ${built}`);
  process.exit(1);
}

mkdirSync(outDir, { recursive: true });
copyFileSync(built, join(outDir, exeName));
copyFileSync(join(root, "LICENSE"), join(outDir, "LICENSE.txt"));
// one readme in the three interface languages; the BOM lets older
// editors pick UTF-8
writeFileSync(
  join(outDir, "README.txt"),
  String.fromCharCode(0xfeff) +
    [
      `Easy SCS Mod Manager ${version} (portable)`,
      "English / 한국어 / 简体中文",
      "",
      "== English ==",
      "",
      `No installation needed. Run ${exeName}.`,
      "",
      "- Settings, category pins, favourites, presets, profile backups and caches are",
      "  all kept in the data folder inside this folder. Move the whole folder",
      "  and everything moves with it.",
      "- Deleting the data folder resets the app. Profile backups go with it.",
      "- When started from a place it cannot write to (Program Files etc.),",
      "  the app uses %APPDATA%\\easy-scsmodmanager-rs instead.",
      "- The Microsoft Edge WebView2 runtime is required. Windows 11 has it.",
      "- The language follows Windows; change it under Settings.",
      "",
      "== 한국어 ==",
      "",
      `설치가 필요 없습니다. ${exeName} 를 실행하세요.`,
      "",
      "- 설정, 분류 고정, 즐겨찾기, 프리셋, 프로필 백업, 캐시는 모두 이 폴더의",
      "  data 폴더에 저장됩니다. 폴더를 통째로 옮기면 그대로 따라갑니다.",
      "- data 폴더를 지우면 초기 상태로 돌아갑니다. 프로필 백업도 함께 지워집니다.",
      "- 폴더에 쓸 수 없는 위치(Program Files 등)에서 실행하면 사용자 폴더",
      "  (%APPDATA%\\easy-scsmodmanager-rs)를 대신 씁니다.",
      "- Microsoft Edge WebView2 런타임이 필요합니다. Windows 11에는 기본으로",
      "  들어 있습니다.",
      "- 언어는 Windows 설정을 따르며, 설정에서 바꿀 수 있습니다.",
      "",
      "== 简体中文 ==",
      "",
      `无需安装，直接运行 ${exeName} 即可。`,
      "",
      "- 设置、分类固定、收藏、预设、档案备份和缓存都保存在本文件夹内的 data",
      "  文件夹中。移动整个文件夹时，这些数据会一起移动。",
      "- 删除 data 文件夹会恢复到初始状态，档案备份也会一并删除。",
      "- 如果从无法写入的位置（如 Program Files）运行，程序会改用",
      "  %APPDATA%\\easy-scsmodmanager-rs。",
      "- 需要 Microsoft Edge WebView2 运行时。Windows 11 已自带。",
      "- 界面语言跟随 Windows，可以在“设置”中更改。",
      "",
      "GPL-3.0-or-later. A Rust rewrite of Switch-Bros/easy-scsmodmanager.",
      "https://github.com/doveis99/easy-scsmodmanager-rs",
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
