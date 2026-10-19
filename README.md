# data_relay (CSV 수집 에이전트)

Windows에서 CSV 파일의 추가 바이트를 감지해 gzip 조각으로 저장하고 UNC 공유 폴더에 전송하는 Rust 애플리케이션입니다. 구현 기준은 [사양서](csv-agent-spec.md)와 [설계서](csv-agent-design.md)입니다. 서버 적재기는 운영용이 아니라 [QuestDB 적재 예제](#서버-적재-예제-questdb)로만 제공합니다.

## 빌드

Windows x64, Rust 1.93 이상 MSVC 도구 체인과 Visual Studio C++ Build Tools/Windows SDK가 필요합니다. 개발 검증 환경은 Rust 1.93.1입니다.

```powershell
cargo build --workspace
cargo test --workspace
cargo build --release -p data_relay -p data_relay_cli
```

배포 실행 파일은 `target/release/data_relay.exe`, 콘솔 검증 도구는 `target/release/data_relay_cli.exe`입니다. `Cargo.lock`을 함께 관리해 같은 의존성 버전으로 빌드합니다.

이전 이름은 `csv-agent.exe`였습니다. 이미 설치된 PC와 호환되도록 데이터 폴더(`%ProgramData%\csv-agent`), 자동 시작 등록 이름(`csv-agent`), 중복 실행 방지 이름은 그대로 씁니다. 그래서 `data_relay.exe`로 교체해도 기존 설정과 전송 기록을 그대로 이어 쓰고, 첫 실행 때 자동 시작 경로가 새 실행 파일로 바뀝니다. 옛 실행 파일은 트레이에서 종료한 뒤 지우세요.

## 실행

```powershell
# 트레이 앱. 설정이 미완료이면 설정 창이 열립니다.
.\target\release\data_relay.exe

# 별도 데이터 디렉토리로 실행하고 설정 창 표시
.\target\release\data_relay.exe --data-dir .\.local\agent --show
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

실행 중 파일을 직접 수정한 내용은 자동으로 다시 읽지 않습니다. 파일을 고친 뒤 화면 아래의 **파일에서 다시 읽기**를 누르면 `config.toml`을 다시 읽어 바로 적용합니다. 화면에서 저장하지 않은 변경이 있으면 사라진다는 확인 창이 먼저 뜹니다. 파일이 없거나 형식이 깨졌으면 현재 설정을 유지하고 오류를 보여 줍니다. 값에 오류가 있으면 화면에만 불러오고 적용하지 않으니, 고친 뒤 **저장 및 적용**을 누르세요. 스풀 경로·로그 경로·로그 보관 개수는 다시 읽어도 재시작 후 반영됩니다. `config.toml`에는 설정만 들어 있습니다. 전송 진행 상태·실행 이력과 미전송 파일은 설정 파일 복사에 포함되지 않습니다.

## 콘솔 검증

```powershell
cargo run -p data_relay_cli -- --data-dir .\.local\agent check-config
cargo run -p data_relay_cli -- scan --pattern 'D:/logs/**/*.csv'
cargo run -p data_relay_cli -- chunk-name --parse 'data.csv__0-1024__hdr.csv.gz'
cargo run -p data_relay_cli -- test-unc --base '\\server\share'
cargo run -p data_relay_cli -- --data-dir .\.local\agent run --dry-run
cargo run -p data_relay_cli -- --data-dir .\.local\agent run
```

`run`은 Ctrl+C로 정상 종료합니다. 자동 검증에서는 `run --duration-sec 10`처럼 실행 시간을 지정할 수 있습니다. `--dry-run`은 감시·디바운스 이벤트를 로그로 확인하는 용도입니다.

실제 공유 폴더 없이 시험하려면 설정의 `server.unc_base`를 임시 로컬 절대경로로 지정하고 CLI에 `--allow-local-target`을 추가하세요. GUI의 운영 설정 검증은 UNC 경로만 허용합니다.

```powershell
cargo run -p data_relay_cli -- --data-dir .\.local\agent --allow-local-target run --duration-sec 10
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

조각 이름은 원본 파일 이름의 대소문자를 그대로 씁니다(`LineA~Vib_001.CSV__0-38__hdr.csv.gz`). 이전 버전은 이름을 소문자로 바꿔 기록했는데, 그 기록이 남은 파일도 새 버전에서 처음 처리할 때부터 원래 대소문자로 바뀝니다. 이미 공유 폴더에 올라간 소문자 조각은 그대로 남으므로, 원래 이름으로 다시 받으려면 아래 offset 초기화로 다시 보내세요.

### 다시 전송하기 (offset 초기화)

에이전트는 파일마다 전송한 위치(offset)를 로컬 `state.db`에 기록하고, 공유 폴더에 올린 조각이 남아 있는지는 확인하지 않습니다. 공유 폴더나 스풀의 조각을 지워도 자동으로 다시 보내지 않습니다. 다시 보내려면 **파일 감시 및 전송** 탭의 **감시 파일** 표에서 초기화합니다.

- 각 행의 **offset 리셋**: 그 파일만 처음부터 다시 업로드합니다. 원본 파일을 찾지 못한 행은 비활성입니다.
- 표 아래 **전체 offset 리셋…**: 현재 있는 감시 파일 전부를 처음부터 다시 업로드합니다.

둘 다 확인 창을 거칩니다. 다시 만든 조각은 이전과 같은 파일 이름으로 **다시 만든 날짜의 폴더**(`{YYYYMMDD}`)에 올라가며, 같은 폴더에 같은 이름이 있으면 덮어씁니다. 이미 적재한 데이터는 QuestDB 적재 예제의 중복 제거 키가 흡수합니다. 단, 같은 날 다시 보낸 조각은 적재 예제의 `--state` 파일에 이미 기록된 경로라 건너뛰므로, 적재까지 다시 하려면 해당 줄이나 파일을 지우세요.

전송 결과는 `{unc_base}/raw/{equip_id}/{YYYYMMDD}/*.csv.gz`, 상태는 `{unc_base}/_status/{equip_id}.json`에 기록됩니다. 조각 날짜는 생성 시점의 로컬 날짜이며, 스풀에 남은 조각의 장비 ID는 전송 시점의 설정을 사용합니다.

## 서버 적재 예제 (QuestDB)

에이전트가 공유 폴더에 올린 조각을 QuestDB에 넣는 Python 예제가 `examples/`에 있습니다.

| 파일 | 내용 |
|---|---|
| [examples/load_to_questdb.py](examples/load_to_questdb.py) | 공유 폴더의 조각을 모두 읽어 QuestDB에 적재 (`questdb.ingress.Sender`) |
| [examples/read_incremental_csv.py](examples/read_incremental_csv.py) | 조각 하나를 읽어 컬럼명과 값을 출력하는 최소 예제 |

### 사용법

Python 3.10 이상이 필요합니다.

```powershell
pip install "questdb>=2,<4"
python examples\load_to_questdb.py \\server\share --host 192.168.88.250
```

| 옵션 | 기본값 | 설명 |
|---|---|---|
| `share` (필수) | | 에이전트의 `unc_base`와 같은 공유 루트. 아래에 `raw\`, `_status\`가 있어야 합니다 |
| `--host`, `--port` | `192.168.88.250`, `9000` | QuestDB HTTP 포트. 데이터 전송(ILP)과 테이블 생성(SQL)에 같이 씁니다 |
| `--table` | `csv_edge_values` | 적재할 테이블. 없으면 만듭니다 |
| `--state` | `loaded_chunks.txt` | 적재가 끝난 조각 목록. 다음 실행 때 이 조각은 건너뜁니다 |
| `--tz-offset` | `+09:00` | 시간대가 없는 타임스탬프를 해석할 UTC 오프셋 |

새 조각이 계속 쌓이므로 작업 스케줄러 등으로 주기 실행하면 됩니다. 실행할 때마다 `--state`에 없는 조각만 넣습니다. 같은 조각을 다시 넣어도 테이블의 중복 제거 키가 흡수하므로, 전체를 다시 넣고 싶으면 `--state` 파일을 지우고 실행하세요.

### 적재 형식

CSV 한 행(타임스탬프 + 데이터 N개)을 데이터 컬럼마다 한 행씩, N행으로 풀어서 넣습니다.

| 컬럼 | 타입 | 값 |
|---|---|---|
| `timestamp` | TIMESTAMP | CSV 행의 시각 (UTC로 저장) |
| `EQUIP_NUM`, `EQUIP_NAME` | SYMBOL | 에이전트 전역 설정의 장비 번호·장비 이름. 하트비트 `_status\{equip_id}.json`에서 읽습니다 |
| `filename` | SYMBOL | 원본 CSV 파일 이름(확장자 포함, 하위 폴더 제외) |
| `colname` | SYMBOL | CSV 컬럼 이름. 헤더 없는 파일은 `c0`, `c1`, … |
| `value_int` / `value_float` / `value_str` | LONG / DOUBLE / VARCHAR | 정수 → `value_int`, 실수 → `value_float`, 나머지(빈 값 포함) → `value_str`. 셋 중 하나만 채워집니다 |

- 테이블은 `PARTITION BY DAY WAL`이며 `DEDUP UPSERT KEYS(timestamp, EQUIP_NUM, EQUIP_NAME, filename, colname)`로 중복을 흡수합니다.
- 타임스탬프 컬럼은 이름이 `timestamp`, `time`, `ts`, `datetime` 등인 컬럼이고, 없으면 첫 컬럼입니다. `2026-10-16 09:00:00.123`, `2026/10/16 09:00:00`, ISO 8601 형식을 읽습니다.
- 하트비트가 아직 없는 장비의 조각은 건너뛰고 다음 실행 때 다시 시도합니다. 깨진 조각은 오류를 출력하고 넣지 않으며, 나머지 조각은 계속 넣습니다.
- 주의: 같은 장비에서 하위 폴더만 다르고 이름이 같은 파일(`LineA\Vib_001.CSV`, `LineB\Vib_001.CSV`)의 같은 시각 값은 중복 제거 때문에 서로 덮어씁니다. 같은 파일 안에서 같은 시각의 행이 두 번 나와도 하나로 합쳐집니다. `007` 같은 코드는 정수 `7`로 들어갑니다.

조회 예 (원래 CSV 모양으로 펼치기, 한국 시간):

```sql
SELECT to_timezone(timestamp, 'Asia/Seoul') AS kst,
       max(CASE WHEN colname = 'ch1' THEN value_int END)   AS ch1,
       max(CASE WHEN colname = 'ch2' THEN value_float END) AS ch2
FROM csv_edge_values
WHERE EQUIP_NUM = 'A-12' AND filename = 'Vib_001.CSV'
GROUP BY timestamp
ORDER BY timestamp;
```

### 바꾸거나 교체하려면

| 바꿀 것 | 고칠 곳 (`examples/load_to_questdb.py`) |
|---|---|
| QuestDB 주소·테이블 이름 | `--host`, `--port`, `--table` 옵션 (코드 수정 불필요) |
| 테이블 구조·파티션·중복 제거 키 | `TABLE_DDL` |
| 정수·실수·문자열 판정 규칙 (예: `007`을 문자열로 유지) | `classify()` |
| 타임스탬프 컬럼 이름 후보 | `TIMESTAMP_NAMES` |
| 타임스탬프 형식 (예: epoch 숫자) | `parse_timestamp()` |
| 파일 이름에 하위 폴더 포함 | `source_filename()` |

- **테이블 구조를 바꿀 때**: `CREATE TABLE IF NOT EXISTS`라서 이미 있는 테이블은 바뀌지 않습니다. 기존 테이블을 지우거나(`DROP TABLE csv_edge_values;`) `--table`로 새 이름을 지정한 뒤, `--state` 파일을 지우고 다시 실행하세요.
- **다른 DB나 다른 적재 프로그램으로 교체할 때**: 에이전트는 바꿀 필요가 없습니다. 새 프로그램은 다음 계약만 지키면 됩니다.
  1. 공유 폴더의 `raw\{equip_id}\{YYYYMMDD}\*.csv.gz`만 읽습니다. 에이전트가 `.tmp`로 쓴 뒤 이름을 바꾸므로 `.tmp` 파일은 무시합니다.
  2. 파일 이름은 오른쪽부터 `__`로 두 번 나눕니다: `{원본 이름 세그먼트}__{시작}-{끝 offset}__{hdr|syn}.csv.gz`. 원본 이름 세그먼트의 `~`는 하위 폴더 구분자이고, `%7e`는 `~`, `%25`는 `%`입니다. 끝에 붙는 `~g숫자-16진수`(파일 재생성), `~c16진수`(이름 충돌)는 원본 이름이 아닙니다.
  3. 조각의 첫 줄은 항상 헤더입니다. `hdr`는 원본 헤더, `syn`은 에이전트가 만든 `c0,c1,…` 헤더입니다.
  4. 같은 조각이 두 번 올 수 있으므로(최소 한 번 전송) DB 쪽에서 중복을 흡수해야 합니다.
  5. 장비 번호·이름은 `_status\{equip_id}.json`의 `equip_num`, `equip_name`을 씁니다.

## 구현 시 보완한 사항

원본 헤더는 UTF-8 문자열로 강제 변환하지 않고 SQLite BLOB에 저장합니다. 이 방식은 기존 ANSI 인코딩 헤더도 그대로 보존합니다. 설정/상태 파일은 손상이 확인된 경우에만 백업·복구하며, 권한 오류를 손상으로 간주해 상태를 초기화하지 않습니다.

파일 재생성이나 관측된 축소 이후에는 원본 이름 세그먼트에 세대 구분 접미사를 붙입니다. 같은 날짜에 이름과 offset 범위가 반복되어도 이전 조각을 덮어쓰지 않도록 사양의 파일명 규칙을 보완한 것입니다. 경로의 리터럴 `~`와 긴 이름도 구분되도록 처리합니다. 서버에서 파일명을 읽을 때는 오른쪽의 `__offset-offset__hdr|syn.csv.gz`를 기준으로 분리하세요.

주기 명령은 창 없이 정지 상태로 생성한 뒤 Windows Job Object에 등록하고 실행을 재개합니다. 타임아웃과 종료 시 하위 프로세스까지 정리합니다. 출력 이력은 각 스트림의 마지막 16 KiB를 보관합니다.

## 현장 확인

Windows 개발 PC에서 자동 테스트 48개, Clippy 경고 검사, 포맷 검사가 통과했습니다. 테스트 CMD 창의 실제 표시, 정상 종료·시간 초과 출력, 한글 표시, Enter로 닫기와 에이전트 종료 시 정리를 확인했습니다. 기존 GUI 세 탭의 한글 표시, X 버튼 숨김, 두 번째 실행으로 기존 창 열기, 숨긴 상태의 전송 명령·트레이 상태 갱신, 종료 확인도 점검했습니다. 숨김 상태 30.01초 측정에서 프로세스 CPU 시간 증가량은 0초였습니다. 이 짧은 회귀 점검은 아래의 10분 현장 기준을 대체하지 않습니다.

자동 테스트 외에 실제 UNC 공유의 권한·단절 후 복구, 실제 CSV 생성 프로그램과의 공유 읽기, 절전 복귀, 트레이 동작을 확인해야 합니다. 유휴 CPU 0% 판정은 대상 PC에서 파일 변경 없이 10분간 두고 작업 관리자에서 확인하는 기준입니다.

개행이 없는 행이 `max_chunk_bytes`를 넘거나 자동 헤더가 64 KiB를 넘으면 offset을 진행시키지 않습니다. 크기 설정이나 원본 형식을 확인하세요. 파일 내용을 같은 크기로 덮어쓰는 방식은 append-only 전제에 포함되지 않습니다.
