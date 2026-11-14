# Easy SCS Mod Manager (Rust)

[English](README.md) · **한국어** · [简体中文](README.zh-CN.md)

Euro Truck Simulator 2와 American Truck Simulator의 모드와 로드 순서를 관리하는 프로그램입니다.

[Switch-Bros/easy-scsmodmanager](https://github.com/Switch-Bros/easy-scsmodmanager)(Python / PyQt6)를 Rust와 Tauri로 다시 쓴 것입니다. 원본과 같은 GPL-3.0-or-later 라이선스를 따릅니다.

## 받아서 실행하기

[릴리스](https://github.com/doveis99/easy-scsmodmanager-rs/releases)에서 `Easy-SCS-Mod-Manager-<버전>-portable.zip`을 받아 원하는 곳에 풀고 `easy-scsmodmanager.exe`를 실행합니다. 설치는 필요 없습니다.

- Windows 10/11 (64비트) 전용입니다.
- 설정, 분류 고정, 즐겨찾기, 프로필 백업, 캐시는 모두 실행 파일 옆 `data` 폴더에 저장됩니다.
- 쓸 수 없는 위치(Program Files 등)에서 실행하면 `%APPDATA%\easy-scsmodmanager-rs`를 대신 씁니다.
- Microsoft Edge WebView2 런타임이 필요합니다. Windows 11에는 기본으로 들어 있습니다.
- 실행 파일에 코드 서명이 없어 처음 실행할 때 Windows SmartScreen 경고가 나올 수 있습니다.

## 언어

화면은 영어, 한국어, 중국어(간체)를 지원합니다. 처음에는 Windows 언어를 따르고, **설정 → 언어**에서 바꿀 수 있습니다. 바꾸면 바로 적용됩니다.

## 기능

- **모드 목록**: `mod` 폴더와 Steam 창작마당의 모드를 스캔합니다. ZIP, HashFS v1/v2, AEM 아카이브와 압축을 푼 폴더를 읽습니다.
- **자동 분류**: 모드가 선언한 분류에 더해 내용으로 분류를 바로잡습니다. 맵 파일과 물리 정의, 정의 파일이 놓인 폴더, 그 파일이 정의하는 유닛을 봅니다. 유닛은 파일 일부를 직접 읽어 확인하며, 3nK로 뒤섞인 정의 파일, 바이너리 정의 파일, 잠긴 아카이브의 이름 없는 파일도 읽습니다. 매니페스트에 분류가 없거나 선언한 분류를 뒷받침하는 파일이 전혀 없으면 내용이 분류를 정하고, 덜 확실한 경우에는 카드에 제안으로만 표시합니다. `scsmm classify`로 모드별 판정 근거를 볼 수 있습니다.
- **수동 분류**: 모드별로 분류를 직접 고정할 수 있습니다. 여러 가지를 다루는 모드는 분류를 여러 개 지정하고 그중 하나를 대표로 고릅니다. 대표 분류가 로드 순서의 그룹을 정하고, 나머지는 분류 필터와 배지에 쓰입니다.
- **로드 순서**: 그룹별로 묶어 보여 주고, 끌어서 또는 `Alt+↑/↓`로 순서를 바꿉니다.
- **자동 정렬**: 그룹별로 정렬하되, 같은 파일을 가진 모드끼리는 위아래 순서를 뒤집지 않습니다. 정렬해도 어느 모드가 파일을 이기는지는 바뀌지 않습니다.
- **충돌 표시**: 같은 `def/` 파일을 위쪽 모드에 덮어써지는 모드를 표시하고 전체 보고서를 보여 줍니다.
- **호환성 표시**: 모드가 선언한 호환 버전이 현재 게임 버전과 맞지 않으면 알려 줍니다.
- **잠긴 아카이브**: 파일 목록을 숨긴 아카이브는 카드에 `목록 숨김`으로 표시합니다. 게임 자체 아카이브의 경로 목록과 대조해 숨은 파일의 이름을 되찾고, 충돌 표시와 분류 판별에 씁니다. 경로 목록은 `data/cache/gamepaths`에 저장하고 게임이 바뀌면 다시 만듭니다.
- **아카이브 추출**: 모드 상세 보기의 `추출…` 버튼으로 모드의 `.scs` 아카이브를 새 폴더에 풉니다. HashFS v2의 압축 텍스처는 `.dds` 파일로 복원합니다. 잠긴 아카이브는 게임의 경로 목록, 파일 내용에 적힌 경로, 이름 규칙으로 파일 이름을 되찾아 원래 경로에 풀고, 끝내 이름을 모르는 파일은 `_unnamed` 폴더에 `해시.확장자`로 풉니다(확장자는 내용으로 추정). 게임 자체의 아카이브는 명령줄 `scsmm extract`로 풉니다.
- **프로필 저장과 백업**: 저장할 때마다 프로필 폴더 전체를 백업하고, 백업에서 복원할 수 있습니다.
- **창작마당 정보**: 이름이 없는 창작마당 모드의 제목과 미리보기를 Steam 공개 API에서 가져옵니다. 설정에서 끌 수 있습니다.
- **공유**: 로드 순서를 파일로 내보내고 가져옵니다. 다른 사람의 `profile.sii`와 맵 콤보도 읽습니다. 파일 형식은 원본 앱과 같습니다.
- **모드 관리**: 상세 보기, 즐겨찾기, 휴지통으로 삭제, 파일 위치 열기.
- **보기 상태 기억**: 마지막으로 쓴 게임과 프로필, 라이브러리의 정렬 기준과 필터를 다음 실행 때 그대로 되살립니다.

원본에 있지만 옮기지 않은 것: 온라인 공유 코드, 자동 업데이트, 독일어 화면, Linux·macOS 패키징.

## 구조

| 경로 | 내용 |
|---|---|
| `crates/scsmm-core` | 화면과 무관한 모든 로직: 아카이브 읽기, SII 파싱, 스캔, 분류, 로드 순서, 프로필, 공유 |
| `crates/scsmm-cli` | 읽기 전용 명령줄 도구 `scsmm` |
| `src-tauri` | Tauri 앱. 코어를 화면에 명령으로 노출합니다 |
| `ui` | 화면 (TypeScript, Vite) |
| `ui/src/locales` | 화면 문구. 언어마다 파일 하나 |
| `scripts` | 포터블 빌드와 정리 스크립트 |

## 개발

Rust, Node.js, pnpm이 필요합니다.

```bash
pnpm install          # 처음 한 번
pnpm tauri dev        # 개발 모드로 실행
cargo test --workspace
pnpm portable         # 포터블 폴더 만들기 (dist-portable/)
pnpm clean            # target 폴더 정리 (릴리스 빌드 캐시는 남김)
pnpm clean --all      # target 폴더 전체 삭제
```

명령줄 도구는 프로필을 건드리지 않고 스캔 결과와 로드 순서를 보여 줍니다.

```bash
cargo run -p scsmm-cli -- installs
cargo run -p scsmm-cli -- scan --game ets2
cargo run -p scsmm-cli -- order --game ets2
cargo run -p scsmm-cli -- extract <아카이브.scs> <빈 폴더>
```

### 언어 추가하기

한국어(`ui/src/locales/ko.ts`)가 기준입니다. 이 파일을 복사해 값을 번역한 다음, `ui/src/i18n.ts`(`Lang`, `MESSAGES`, `LOCALE`, `LANGS`)와 `crates/scsmm-core/src/settings.rs`의 `Settings::normalize_language`에 새 언어를 등록합니다. 빠진 키가 있으면 빌드가 실패합니다.

## 라이선스

GPL-3.0-or-later. 전문은 [LICENSE](LICENSE)에 있습니다.
