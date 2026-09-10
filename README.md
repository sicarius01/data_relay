# CSV 수집 에이전트

Windows에서 CSV 파일의 추가 바이트를 감지해 gzip 조각으로 저장하고 UNC 공유 폴더에 전송하는 Rust 애플리케이션입니다. 구현 기준은 [사양서](csv-agent-spec.md)와 [설계서](csv-agent-design.md)입니다. 서버 적재기와 QuestDB 연동은 이 프로젝트의 범위 밖입니다.

## 빌드

Windows x64, Rust 1.93 이상 MSVC 도구 체인과 Visual Studio C++ Build Tools/Windows SDK가 필요합니다. 개발 검증 환경은 Rust 1.93.1입니다.

```powershell
cargo build --workspace
cargo test --workspace
cargo build --release -p csv-agent -p csv-agent-cli
```

배포 실행 파일은 `target/release/csv-agent.exe`, 콘솔 검증 도구는 `target/release/csv-agent-cli.exe`입니다. `Cargo.lock`을 함께 관리해 같은 의존성 버전으로 빌드합니다.

## 실행

```powershell
# 트레이 앱. 설정이 미완료이면 설정 창이 열립니다.
.\target\release\csv-agent.exe

# 별도 데이터 디렉토리로 실행하고 설정 창 표시
.\target\release\csv-agent.exe --data-dir .\.local\agent --show
```

첫 실행 때 데이터 폴더에 `config.toml`을 생성합니다. 장비 번호·장비 이름·UNC 경로·감시 패턴을 입력하세요. 장비 번호와 이름의 조합은 PC마다 달라야 합니다. 설정 파일 위치와 복사 방법은 아래를 참고하세요.

UNC 접근에는 로그인한 사용자의 자격 증명을 사용합니다. 수집 대상 프로그램은 파일의 공유 읽기를 허용해야 합니다. 로그인하지 않은 동안에는 트레이 앱이 실행되지 않습니다.

GUI는 전역 설정, 주기 명령 실행, 파일 감시/전송의 세 탭으로 구성됩니다. 창의 X 버튼은 트레이로 숨기며, 트레이 종료 메뉴에서 확인 후 종료합니다. 설정 저장은 실행 중 반영되고, 스풀 경로·로그 경로·로그 보관 개수는 재시작 후 반영됩니다.

전역 설정의 **연결 테스트**는 현재 입력한 경로의 쓰기·이름 변경·삭제 권한을 확인합니다. 입력한 설정을 수집에 사용하려면 **저장 및 적용**을 누르세요. 저장 전에는 상단에 **설정 저장 필요**가 표시됩니다. 실제 감시 오류나 전송 중단은 원인과 함께 별도로 표시됩니다. 이미 적용된 경로의 연결 테스트가 성공하면 중단된 전송을 다시 시도합니다.

주기 명령의 **예약 실행 사용**은 정해진 주기에 자동 실행할지 정하는 옵션입니다. 예약 실행은 창을 숨기고, **테스트 실행 (CMD 창 표시)**은 별도 콘솔 창에 출력과 종료 결과를 표시합니다. 테스트는 현재 입력한 명령을 저장하지 않고 한 번 실행하므로 예약 실행이 꺼져 있어도 사용할 수 있습니다. 완료된 콘솔은 Enter 키로 닫습니다. 실행 결과의 종료 코드·표준 출력·오류 출력은 앱에서도 확인할 수 있습니다.

## 설정 파일 위치와 복사

전역 설정에서 **저장 및 적용**을 누르면 데이터 폴더의 `config.toml`에 저장됩니다. 기본 저장 위치는 다음과 같습니다.

```text
%ProgramData%\csv-agent\config.toml
```

일반적인 Windows 환경에서는 `C:\ProgramData\csv-agent\config.toml`입니다. `ProgramData`는 숨김 폴더일 수 있으므로 탐색기 주소창에 `%ProgramData%\csv-agent`를 입력하면 바로 열 수 있습니다.

프로그램이나 소스 코드 폴더에 있는 [config.example.toml](config.example.toml)은 설정 형식을 보여 주는 예제입니다. 앱에서 저장한 설정은 이 예제 파일에 반영되지 않습니다. 해당 폴더에서 예제 파일만 보이더라도 위 데이터 폴더의 `config.toml`을 확인하세요.

`--data-dir` 옵션을 사용하면 지정한 폴더의 `config.toml`을 읽고 저장합니다. 예를 들어 `--data-dir .\.local\agent`로 실행했다면 실행한 작업 폴더 기준 `.local\agent\config.toml`을 사용합니다.

다른 PC로 설정을 복사하거나 파일을 직접 수정하려면 다음 순서로 진행하세요.

1. 원본 PC에서 설정을 저장한 뒤 데이터 폴더의 `config.toml`을 복사합니다.
2. 대상 PC의 프로그램을 **트레이 메뉴에서 완전히 종료**합니다. 창의 X 버튼은 숨김만 수행합니다.
3. 대상 PC의 데이터 폴더에 파일을 넣습니다. 기존 설정이 있으면 먼저 백업하세요.
4. 파일을 수정해 **장비번호·장비이름 조합이 다른 PC와 중복되지 않도록** 변경하고, UNC 경로·감시 경로·명령 실행 경로·스풀 및 로그 경로를 해당 PC에 맞춥니다.
5. 프로그램을 실행하면 복사하거나 수정한 설정을 읽습니다. 이후 앱에서 수정할 때는 **저장 및 적용**을 누르세요.

실행 중 파일을 직접 수정한 내용은 자동으로 다시 읽지 않으므로, 파일 편집은 프로그램을 종료한 상태에서 하고 다시 실행하세요. `config.toml`에는 설정만 들어 있습니다. 전송 진행 상태·실행 이력과 미전송 파일은 설정 파일 복사에 포함되지 않습니다.

## 콘솔 검증

```powershell
cargo run -p csv-agent-cli -- --data-dir .\.local\agent check-config
cargo run -p csv-agent-cli -- scan --pattern 'D:/logs/**/*.csv'
cargo run -p csv-agent-cli -- chunk-name --parse 'data.csv__0-1024__hdr.csv.gz'
cargo run -p csv-agent-cli -- test-unc --base '\\server\share'
cargo run -p csv-agent-cli -- --data-dir .\.local\agent run --dry-run
cargo run -p csv-agent-cli -- --data-dir .\.local\agent run
```

`run`은 Ctrl+C로 정상 종료합니다. 자동 검증에서는 `run --duration-sec 10`처럼 실행 시간을 지정할 수 있습니다. `--dry-run`은 감시·디바운스 이벤트를 로그로 확인하는 용도입니다.

실제 공유 폴더 없이 시험하려면 설정의 `server.unc_base`를 임시 로컬 절대경로로 지정하고 CLI에 `--allow-local-target`을 추가하세요. GUI의 운영 설정 검증은 UNC 경로만 허용합니다.

```powershell
cargo run -p csv-agent-cli -- --data-dir .\.local\agent --allow-local-target run --duration-sec 10
```

## 데이터 처리

- 파일별 Windows 고유 ID와 바이트 offset을 SQLite에 저장합니다.
- OS 파일 이벤트와 비동기 타이머를 사용합니다. 짧은 주기로 파일을 반복 스캔하지 않습니다.
- 완성된 마지막 개행까지만 조각을 만들며, 미완성 꼬리는 다음 변경 때 처리합니다.
- `auto`는 첫 줄을 헤더로 사용합니다. 첫 조각에서는 원본 헤더를 중복해서 붙이지 않습니다.
- `none`은 `c0,c1,…` 헤더를 만들고, `fixed`는 지정한 컬럼명을 사용합니다. 원본 데이터 바이트는 보존합니다.
- 로컬 `.tmp` 쓰기·동기화·rename 완료 후 offset을 커밋합니다. 전송도 대상 `.tmp`에 쓴 뒤 최종 이름으로 교체합니다.
- 전송 실패 시 지수 백오프 후 대기합니다. 수동 전송·새 조각·설정 변경·하트비트 성공·절전 복귀가 재시도를 깨웁니다.
- 미완료 장비 설정에서는 전송을 보류하고 로컬 수집을 계속합니다. 스풀 용량 상한을 넘으면 사양에 따라 오래된 미전송 조각부터 삭제하며 경고를 남깁니다.

전송 결과는 `{unc_base}/raw/{equip_id}/{YYYYMMDD}/*.csv.gz`, 상태는 `{unc_base}/_status/{equip_id}.json`에 기록됩니다. 조각 날짜는 생성 시점의 로컬 날짜이며, 스풀에 남은 조각의 장비 ID는 전송 시점의 설정을 사용합니다.

## 구현 시 보완한 사항

원본 헤더는 UTF-8 문자열로 강제 변환하지 않고 SQLite BLOB에 저장합니다. 이 방식은 기존 ANSI 인코딩 헤더도 그대로 보존합니다. 설정/상태 파일은 손상이 확인된 경우에만 백업·복구하며, 권한 오류를 손상으로 간주해 상태를 초기화하지 않습니다.

파일 재생성이나 관측된 축소 이후에는 원본 이름 세그먼트에 세대 구분 접미사를 붙입니다. 같은 날짜에 이름과 offset 범위가 반복되어도 이전 조각을 덮어쓰지 않도록 사양의 파일명 규칙을 보완한 것입니다. 경로의 리터럴 `~`와 긴 이름도 구분되도록 처리합니다. 서버에서 파일명을 읽을 때는 오른쪽의 `__offset-offset__hdr|syn.csv.gz`를 기준으로 분리하세요.

주기 명령은 창 없이 정지 상태로 생성한 뒤 Windows Job Object에 등록하고 실행을 재개합니다. 타임아웃과 종료 시 하위 프로세스까지 정리합니다. 출력 이력은 각 스트림의 마지막 16 KiB를 보관합니다.

## 현장 확인

Windows 개발 PC에서 자동 테스트 48개, Clippy 경고 검사, 포맷 검사가 통과했습니다. 테스트 CMD 창의 실제 표시, 정상 종료·시간 초과 출력, 한글 표시, Enter로 닫기와 에이전트 종료 시 정리를 확인했습니다. 기존 GUI 세 탭의 한글 표시, X 버튼 숨김, 두 번째 실행으로 기존 창 열기, 숨긴 상태의 전송 명령·트레이 상태 갱신, 종료 확인도 점검했습니다. 숨김 상태 30.01초 측정에서 프로세스 CPU 시간 증가량은 0초였습니다. 이 짧은 회귀 점검은 아래의 10분 현장 기준을 대체하지 않습니다.

자동 테스트 외에 실제 UNC 공유의 권한·단절 후 복구, 실제 CSV 생성 프로그램과의 공유 읽기, 절전 복귀, 트레이 동작을 확인해야 합니다. 유휴 CPU 0% 판정은 대상 PC에서 파일 변경 없이 10분간 두고 작업 관리자에서 확인하는 기준입니다.

개행이 없는 행이 `max_chunk_bytes`를 넘거나 자동 헤더가 64 KiB를 넘으면 offset을 진행시키지 않습니다. 크기 설정이나 원본 형식을 확인하세요. 파일 내용을 같은 크기로 덮어쓰는 방식은 append-only 전제에 포함되지 않습니다.
