# Easy SCS Mod Manager (Rust)

Euro Truck Simulator 2와 American Truck Simulator의 모드와 로드 순서를 관리하는 프로그램입니다.

[Switch-Bros/easy-scsmodmanager](https://github.com/Switch-Bros/easy-scsmodmanager)(Python / PyQt6)를 Rust와 Tauri로 다시 쓴 것입니다. 원본과 같은 GPL-3.0-or-later 라이선스를 따릅니다.

## 실행

설치가 필요 없는 포터블 방식입니다.

```bash
pnpm portable
```

`dist-portable/Easy SCS Mod Manager/` 폴더가 만들어집니다. 그 안의 `easy-scsmodmanager.exe`를 실행하면 됩니다. 폴더는 어디로 옮겨도 됩니다.

- 설정, 분류 고정, 즐겨찾기, 프로필 백업, 창작마당 캐시는 모두 실행 파일 옆 `data` 폴더에 저장됩니다.
- 쓸 수 없는 위치(Program Files 등)에서 실행하면 `%APPDATA%\easy-scsmodmanager-rs`를 대신 씁니다.
- Microsoft Edge WebView2 런타임이 필요합니다. Windows 11에는 기본으로 들어 있습니다.

## 기능

- **모드 목록**: `mod` 폴더와 Steam 창작마당의 모드를 스캔합니다. ZIP, HashFS v1/v2, AEM 아카이브와 압축을 푼 폴더를 읽습니다.
- **자동 분류**: 모드가 선언한 분류에 더해 내용(맵 파일, 물리 정의)으로 분류를 바로잡습니다. 모드별로 분류를 직접 고정할 수도 있습니다.
- **로드 순서**: 그룹별로 묶어 보여 주고, 끌어서 또는 `Alt+↑/↓`로 순서를 바꿉니다.
- **자동 정렬**: 그룹별로 정렬하되, 같은 파일을 가진 모드끼리는 위아래 순서를 뒤집지 않습니다. 정렬해도 어느 모드가 파일을 이기는지는 바뀌지 않습니다.
- **충돌 표시**: 같은 `def/` 파일을 위쪽 모드에 덮어써지는 모드를 표시하고 전체 보고서를 보여 줍니다.
- **호환성 표시**: 모드가 선언한 호환 버전이 현재 게임 버전과 맞지 않으면 알려 줍니다.
- **프로필 저장과 백업**: 저장할 때마다 프로필 폴더 전체를 백업하고, 백업에서 복원할 수 있습니다.
- **창작마당 정보**: 이름이 없는 창작마당 모드의 제목과 미리보기를 Steam 공개 API에서 가져옵니다. 설정에서 끌 수 있습니다.
- **공유**: 로드 순서를 파일로 내보내고 가져옵니다. 다른 사람의 `profile.sii`와 맵 콤보도 읽습니다. 파일 형식은 원본 앱과 같습니다.
- **모드 관리**: 상세 보기, 즐겨찾기, 휴지통으로 삭제, 파일 위치 열기.

원본에 있지만 옮기지 않은 것: 온라인 공유 코드, 자동 업데이트, 영어·독일어 화면, Linux·macOS 패키징.

## 구조

| 경로 | 내용 |
|---|---|
| `crates/scsmm-core` | 화면과 무관한 모든 로직: 아카이브 읽기, SII 파싱, 스캔, 분류, 로드 순서, 프로필, 공유 |
| `crates/scsmm-cli` | 읽기 전용 명령줄 도구 `scsmm` |
| `src-tauri` | Tauri 앱. 코어를 화면에 명령으로 노출합니다 |
| `ui` | 화면 (TypeScript, Vite) |
| `scripts` | 포터블 빌드와 정리 스크립트 |

## 개발

Rust, Node.js, pnpm이 필요합니다.

```bash
pnpm install          # 처음 한 번
pnpm tauri dev        # 개발 모드로 실행
cargo test --workspace
pnpm portable         # 포터블 폴더 만들기
pnpm clean            # target 폴더 정리 (릴리스 빌드 캐시는 남김)
pnpm clean --all      # target 폴더 전체 삭제
```

명령줄 도구는 프로필을 건드리지 않고 스캔 결과와 로드 순서를 보여 줍니다.

```bash
cargo run -p scsmm-cli -- installs
cargo run -p scsmm-cli -- scan --game ets2
cargo run -p scsmm-cli -- order --game ets2
```

## 라이선스

GPL-3.0-or-later. 전문은 [LICENSE](LICENSE)에 있습니다.
