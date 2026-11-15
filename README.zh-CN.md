# Easy SCS Mod Manager (Rust)

[English](README.md) · [한국어](README.ko.md) · **简体中文**

用于管理《欧洲卡车模拟 2》（Euro Truck Simulator 2）和《美国卡车模拟》（American Truck Simulator）的模组与加载顺序的工具。

本项目用 Rust 和 Tauri 重写了 [Switch-Bros/easy-scsmodmanager](https://github.com/Switch-Bros/easy-scsmodmanager)（Python / PyQt6），沿用相同的 GPL-3.0-or-later 许可证。

## 下载与运行

从[发布页](https://github.com/doveis99/easy-scsmodmanager-rs/releases)下载 `Easy-SCS-Mod-Manager-<版本>-portable.zip`，解压到任意位置后运行 `easy-scsmodmanager.exe`。无需安装。

- 仅支持 Windows 10/11（64 位）。
- 设置、分类固定、收藏、预设、档案备份和缓存都保存在可执行文件旁边的 `data` 文件夹中。
- 如果从无法写入的位置（如 Program Files）运行，程序会改用 `%APPDATA%\easy-scsmodmanager-rs`。
- 需要 Microsoft Edge WebView2 运行时。Windows 11 已自带。
- 可执行文件没有代码签名，首次运行时 Windows SmartScreen 可能会发出警告。

## 语言

界面支持英语、韩语和简体中文。首次运行时跟随 Windows 的语言，可以在**设置 → 语言**中更改，更改后立即生效。

## 功能

- **模组列表**：扫描 `mod` 文件夹和 Steam 创意工坊中的模组。可读取 ZIP、HashFS v1/v2、AEM 压缩包以及已解压的文件夹。
- **自动分类**：除了模组自己声明的分类，还会根据内容修正分类：地图文件和物理定义、定义文件所在的文件夹，以及这些文件定义的单元。单元是抽取部分文件读取得到的，包括用 3nK 打乱的定义文件、二进制定义文件和锁定压缩包中没有名称的文件。清单没有声明分类，或者声明的分类完全没有文件支持时，由内容决定分类；不够确定的结果只在卡片上显示为建议。`scsmm classify` 会列出每个模组的判定依据。
- **手动分类**：可以为每个模组手动固定分类。涉及多个方面的模组可以指定多个分类，并选择其中一个作为主分类。主分类决定它在加载顺序中的分组，其余的用于分类筛选和标签。
- **加载顺序**：按分组显示，可以拖动或按 `Alt+↑/↓` 调整顺序。
- **自动排序**：按分组排序，但不会颠倒含有相同文件的模组之间的上下顺序，因此排序不会改变哪个模组的文件生效。
- **冲突提示**：标出 `def/` 文件被上方模组覆盖的模组，并提供完整报告。
- **关联模组**：在加载顺序中选中一个模组，会标出它的补丁、附加模组以及它所依托的基础模组。如果一个模组覆盖了只有另一个模组才有的文件（游戏本身没有的文件），就把它看作那个模组的补丁。停用被其他模组依托的模组时，会询问是一并停用它们、只停用这个模组，还是取消。
- **兼容性提示**：模组声明的兼容版本与当前游戏版本不符时给出提示。
- **锁定的压缩包**：隐藏了文件列表的压缩包会在卡片上标为“隐藏列表”。程序会对照游戏自身压缩包的路径列表找回隐藏文件的名称，并用于冲突检测和分类判断。路径列表缓存在 `data/cache/gamepaths` 中，游戏更新后会重新生成。
- **解包**：在模组详情中点击“解包…”，可以把模组的 `.scs` 压缩包解到新文件夹。HashFS v2 的压缩纹理会还原为 `.dds` 文件。对于锁定的压缩包，会通过游戏的路径列表、文件内容中出现的路径以及命名规则找回文件名并解到原路径；仍然无法确定名称的文件会以 `哈希值.扩展名` 的形式解到 `_unnamed` 文件夹（扩展名根据内容推测）。游戏自身的压缩包可以用命令行 `scsmm extract` 解包。
- **保存与备份**：每次保存前都会备份整个档案文件夹，并可以从备份恢复。
- **预设**：给加载顺序起名并按游戏分别保存（卡车、客车、轿车等），在加载顺序上方的列表中选择即可切换。应用预设后，只有点击“保存”才会修改档案。
- **复制、粘贴模组**：在加载顺序中点击（按住 Ctrl 或 Shift 可多选）选中模组并复制，再把其他预设或档案中没有的模组粘贴到各自的分组（Ctrl+C / Ctrl+V）。
- **创意工坊信息**：从 Steam 公开 API 获取没有名称的创意工坊模组的标题和预览图。可以在设置中关闭。
- **更新**：启动时向 GitHub 查询是否有新版本，有的话在上方显示 **更新** 按钮。点击后会下载新版本，与发行页面给出的 SHA-256 核对，替换程序文件并重新启动；`data` 文件夹保持原样。可以在设置中关闭检查，未经点击不会安装任何东西。
- **分享**：以文件形式导出和导入加载顺序，也能读取别人的 `profile.sii` 和地图组合。文件格式与原版程序相同。
- **模组管理**：查看详情、收藏、移到回收站、打开文件位置。
- **记住视图**：下次启动时恢复上次使用的游戏和档案，以及模组库的排序方式和筛选条件。

原版中有但未移植的功能：在线分享码、德语界面、Linux 和 macOS 打包。

## 目录结构

| 路径 | 内容 |
|---|---|
| `crates/scsmm-core` | 所有与界面无关的逻辑：读取压缩包、解析 SII、扫描、分类、加载顺序、档案、分享 |
| `crates/scsmm-cli` | 只读的命令行工具 `scsmm` |
| `src-tauri` | Tauri 应用，把核心功能以命令的形式提供给界面 |
| `ui` | 界面（TypeScript、Vite） |
| `ui/src/locales` | 界面文本，每种语言一个文件 |
| `scripts` | 便携版构建脚本和清理脚本 |

## 开发

需要 Rust、Node.js 和 pnpm。

```bash
pnpm install          # 首次执行一次
pnpm tauri dev        # 以开发模式运行
cargo test --workspace
pnpm portable         # 生成便携版文件夹（dist-portable/）
pnpm clean            # 清理 target 文件夹（保留发布构建缓存）
pnpm clean --all      # 删除整个 target 文件夹
```

命令行工具只显示扫描结果和加载顺序，不会修改档案。

```bash
cargo run -p scsmm-cli -- installs
cargo run -p scsmm-cli -- scan --game ets2
cargo run -p scsmm-cli -- classify --game ets2
cargo run -p scsmm-cli -- order --game ets2
cargo run -p scsmm-cli -- extract <压缩包.scs> <空文件夹>
```

### 添加语言

以韩语（`ui/src/locales/ko.ts`）为基准。复制该文件并翻译其中的值，然后在 `ui/src/i18n.ts`（`Lang`、`MESSAGES`、`LOCALE`、`LANGS`）和 `crates/scsmm-core/src/settings.rs` 的 `Settings::normalize_language` 中登记新语言。缺少任何键都会导致构建失败。

## 许可证

GPL-3.0-or-later。全文见 [LICENSE](LICENSE)。
