# CSV 수집 에이전트 (엣지) 설계서

**버전** 0.1 (초안)
**작성일** 2026-09-10
**대상 사양** `csv-agent-spec.md` v0.3
**수준** 모듈 설계. 크레이트/모듈 구조, 핵심 타입, 채널 메시지, 워커별 상태 머신과 시퀀스,
에러 처리·복구 시나리오까지 다룬다. 함수 본문은 다루지 않는다.

### 이 문서를 읽는 법

- 사양서의 절 번호를 `[spec 2.4]` 형태로 인용한다.
- 사양서에 없는 사항을 이 문서에서 정한 경우 **[설계 결정]** 으로 표시하고, 12장에 근거를 모아 둔다.
- 사양서 자체를 고쳐야 하는 항목은 13장에 따로 모았다.

### 확정된 전제

| 항목 | 결정 |
|---|---|
| 기술 스택 | 사양 5.3 제안 그대로 확정. Rust + tokio + notify + eframe/egui + tray-icon + rusqlite |
| 설정 변경 반영 | GUI에서 저장하면 프로세스 재시작 없이 즉시 반영. 예외는 3.1.5 참고 |
| 사양 미명시 항목 | 이 문서에서 결정하고 표시 |

---

## 1. 전체 구조

### 1.1 크레이트 구성

Cargo 워크스페이스로 나눈다. 배포물은 `csv-agent.exe` 하나다.

```
csv-agent/
├── Cargo.toml                 # [workspace]
├── crates/
│   ├── core/                  # lib  csv_agent_core — GUI와 무관한 모든 로직
│   ├── cli/                   # bin  csv-agent-cli  — 헤드리스 검증용 (사양 9장 1~6단계)
│   └── app/                   # bin  csv-agent      — 트레이 + egui GUI (배포 실행 파일)
└── assets/
    └── icons/                 # 트레이 아이콘 3종 (정상/대기/오류) PNG → build 시 RGBA 임베드
```

**core를 라이브러리로 분리하는 이유**: 사양 9장이 "1~6까지 완성되면 콘솔 앱 형태로 실사용
검증"을 요구한다. GUI 없이 동일한 런타임을 띄우는 `cli` 바이너리가 그 역할을 하며, 두 바이너리는
같은 `core::runtime::start()`를 호출한다. 테스트도 core만으로 돌아간다.

### 1.2 core 모듈 구성

```
csv_agent_core
├── config        Config 타입, 로드/검증/저장, equip_id 계산, 변경 diff
├── paths         경로 정규화, glob 고정 prefix 추출, 이름 치환(sanitize), 조각 파일명 생성/파싱
├── state         SQLite 접근 (progress, task_history, counters, meta)
├── bus           Command / StatusSnapshot / 내부 채널 메시지 타입
├── shared        SharedState — 워커가 갱신하고 GUI·하트비트가 읽는 런타임 상태
├── watch         Watcher — notify 감시, 패턴 필터, 초기 스캔, rescan
├── debounce      Debouncer — 파일별 타이머 재설정, max_wait 강제 처리
├── chunk         Chunker — 증분 읽기, 개행 잘라내기, 헤더 처리, gzip, 스풀 저장, offset 커밋
├── spool         Spool — 디렉토리 레이아웃, 열거, 디스크 가드
├── upload        Uploader — UNC 전송, .tmp→rename, 백오프, 게이팅
├── task          Scheduler + Runner — 주기 명령 실행
├── heartbeat     Heartbeat — 상태 JSON 작성
├── logging       tracing 초기화, 레벨 reload 핸들
├── runtime       AgentRuntime — tokio 런타임 생성, 워커 spawn, AgentHandle 제공, 종료
└── win           Windows 전용: 파일 고유 ID, CREATE_NO_WINDOW, Job Object, 레지스트리 Run,
                  named mutex/event, 절전 복귀 알림
```

app 모듈 구성:

```
csv_agent (app)
├── main          named mutex 확인 → 로그 초기화 → runtime 시작 → eframe 실행
├── tray          TrayIcon 생성, 메뉴, 아이콘/툴팁 갱신, 이벤트 → UiEvent 변환
├── ui
│   ├── app       eframe::App 구현. 창 표시/숨김, 탭 라우팅, 모달 관리
│   ├── tab_global   탭 1 전역 설정
│   ├── tab_tasks    탭 2 주기 명령 실행
│   ├── tab_watch    탭 3 파일 감시 및 전송
│   ├── draft        ConfigDraft — 편집 중인 설정 (문자열 필드) ↔ Config 변환/검증
│   └── modal        종료 확인, offset 리셋 확인, 오류 표시
└── single_instance  두 번째 인스턴스가 기존 창을 띄우는 named event 처리
```

### 1.3 프로세스·스레드 모델 [spec 5.2]

```
┌─ 메인 스레드 ─────────────────────────────────────────────┐
│  winit 이벤트 루프 (eframe)  +  tray-icon 메시지 처리        │
│  ControlFlow::Wait — 이벤트가 없으면 GetMessage에서 블록     │
└───────────────┬───────────────────────────▲───────────────┘
                │ Command (mpsc)             │ StatusSnapshot (watch) + repaint 훅
┌───────────────▼───────────────────────────┴───────────────┐
│  "agent-runtime" std 스레드 → tokio multi_thread 런타임     │
│    worker_threads = 2                                      │
│                                                            │
│   Watcher ─▶ Debouncer ─▶ Chunker ─▶ [spool] ─▶ Uploader   │
│                                              ▲             │
│   Scheduler(task별 루프)     Heartbeat ───────┘ (nudge)     │
│                                                            │
│   블로킹 IO(SQLite, 파일 읽기, gzip, SMB)는 spawn_blocking  │
└────────────────────────────────────────────────────────────┘
   notify 내부 스레드 (ReadDirectoryChangesW 완료 대기)
   tracing-appender 쓰기 스레드 (채널 recv 대기)
   single-instance 대기 스레드 (WaitForSingleObject INFINITE)
```

- tokio 런타임은 GUI를 방해하지 않도록 별도 std 스레드에서 `block_on`으로 돌린다.
- 유휴 상태에서 위 모든 스레드는 커널 대기 상태다 (IOCP, 타이머, 채널 recv, 이벤트 객체).
  주기적으로 깨어나는 루프는 하트비트(기본 5분)와 스케줄러(다음 실행 시각)뿐이다. 9장 참고.

### 1.4 데이터 흐름

```
파일 변경 (OS 이벤트)
  │ notify 콜백 스레드: 패턴 필터 → FsEvent::Changed(path)
  ▼
Debouncer          파일별 타이머 재설정. 조용해진 뒤 debounce_sec 또는 첫 이벤트+max_wait
  │ ProcessRequest{path, reason}
  ▼
Chunker (순차)     file_key 확인 → offset/size 비교 → 헤더 준비 → 읽기 → 마지막 개행 절단
  │                → gzip → spool/{date}/{name}.tmp → fsync → rename → offset 커밋
  │ Notify
  ▼
Uploader           spool 열거 → UNC {name}.tmp 쓰기 → rename → 로컬 삭제 → 통계 갱신
  │ 실패 시 지수 백오프, retry_max 초과 시 Suspended (nudge 대기)
  ▼
\\server\share\raw\{equip_id}\{YYYYMMDD}\{name}
```

---

## 2. 공통 타입과 채널

### 2.1 GUI ↔ 런타임 경계

core는 GUI를 모른다. 경계는 `AgentHandle` 하나다.

```rust
pub struct AgentHandle {
    cmd_tx:    mpsc::UnboundedSender<Command>,
    status_rx: watch::Receiver<Arc<StatusSnapshot>>,
    config_rx: watch::Receiver<Arc<Config>>,
}

pub fn start(
    config: Config,
    paths: AppPaths,                                // config/state/spool/log 경로 묶음
    on_change: Arc<dyn Fn() + Send + Sync>,         // 상태 변경 시 호출. app에서는 ctx.request_repaint()
) -> Result<AgentHandle, StartError>;
```

```rust
pub enum Command {
    ReloadConfig(Config),                           // 검증 완료된 새 설정
    FlushNow,                                       // 트레이 "지금 전송" / 탭 3 버튼
    ProcessFileNow(PathBuf),                        // 디바운스 없이 즉시 처리 (테스트/디버그)
    ResetOffset(FileKey),                           // 탭 3 offset 리셋 (확인 후)
    RunTaskNow(String),                             // 탭 2 즉시 실행
    TestUnc { base: String, reply: oneshot::Sender<Result<(), String>> },
    PreviewPattern { pattern: String, reply: oneshot::Sender<Result<Vec<PathBuf>, String>> },
    TaskHistory { name: String, reply: oneshot::Sender<Vec<TaskRun>> },
    SystemResumed,                                  // 절전 복귀 알림 (win::power)
    Shutdown,
}
```

`StatusSnapshot`은 불변 값이다. GUI는 `status_rx.borrow()`로 최신본을 읽어 그리기만 한다.

```rust
pub struct StatusSnapshot {
    pub generated_at:     DateTime<Local>,
    pub equip_id:         Option<String>,           // 검증 실패면 None
    pub tray:             TrayState,                // Ok | Pending | Error
    pub config_errors:    Vec<String>,              // 상단 경고 배너용
    pub files:            Vec<FileStatus>,          // 탭 3 현황 테이블
    pub watch_errors:     Vec<WatchError>,          // 패턴별 감시 실패 사유
    pub spool_pending:    u64,
    pub spool_bytes:      u64,
    pub bytes_sent_today: u64,
    pub last_upload_at:   Option<DateTime<Local>>,
    pub last_error:       Option<ErrorInfo>,        // {at, component, message}
    pub uploader:         UploaderState,            // Idle | Draining | Backoff{until, attempt} | Suspended | Gated
    pub tasks:            Vec<TaskStatus>,          // name, enabled, next_run, running, last: Option<TaskRun>
}

pub struct FileStatus {
    pub file_key: FileKey, pub path: PathBuf, pub size: u64, pub offset: u64,
    pub header_kind: Option<HeaderKind>, pub header_mode: HeaderMode,
    pub last_processed: Option<DateTime<Local>>, pub present: bool,   // 초기 스캔에서 못 찾으면 false
    pub last_error: Option<String>,
}
```

`TrayState` 도출 규칙 [spec 5.4]:

| 조건 (위에서부터 첫 일치) | 상태 |
|---|---|
| `config_errors`가 비어 있지 않음, 또는 `uploader == Suspended`, 또는 `watch_errors`가 비어 있지 않음 | Error |
| `spool_pending > 0` | Pending |
| 그 외 | Ok |

### 2.2 내부 채널

| 구간 | 타입 | 메시지 | 비고 |
|---|---|---|---|
| notify 콜백 → Debouncer | `mpsc::UnboundedSender<FsEvent>` | `Changed(path)` / `Removed(path)` | 콜백은 동기 컨텍스트라 unbounded 사용 |
| notify 콜백 → Watcher rescan 태스크 | `Arc<Notify>` | — | 버퍼 오버플로(`need_rescan`) 시 전체 스캔 요청 |
| Debouncer → Chunker | `mpsc::Sender<ProcessRequest>` (cap 1024) | `Process{path, reason: Quiet\|MaxWait\|Manual}` / `Removed(path)` (초기 스캔·rescan 결과는 `Changed`로 들어와 Quiet가 된다) | Chunker는 순차 처리. `Removed`는 DB 행 제거 |
| Chunker → Uploader | `tokio::sync::Notify` | — | 조각이 스풀에 추가됨 |
| Heartbeat → Uploader / Watcher | `Notify` | — | UNC 성공 시 uploader nudge, 감시 실패 패턴 재시도 |
| 런타임 → 모든 워커 | `watch::Receiver<Arc<Config>>` | 새 설정 | 각 워커가 `changed().await`로 select |
| 런타임 → 모든 워커 | `CancellationToken` (tokio-util) | — | 종료 |
| 워커 → GUI | `watch::Sender<Arc<StatusSnapshot>>` + `on_change` 훅 | 스냅샷 | 변경이 있을 때만 publish |

### 2.3 SharedState

워커가 갱신하고 스냅샷의 원천이 되는 단일 상태. `Arc<Mutex<Inner>>` 로 두고, 모든 갱신은
`update(|s| ...)` 를 통해 이루어지며 그 끝에서 자동으로 `publish()` 한다.

```rust
pub struct SharedState { inner: Mutex<Inner>, status_tx: watch::Sender<Arc<StatusSnapshot>>, on_change: Arc<dyn Fn()+Send+Sync> }

struct Inner {
    config:          Arc<Config>,
    equip_id:        Result<String, Vec<String>>,   // 검증 결과 캐시
    files:           HashMap<FileKey, FileStatus>,  // 시작 시 DB에서 적재, 이후 Chunker가 갱신
    watch_errors:    Vec<WatchError>,
    spool:           SpoolStats,                    // pending, bytes (Spool이 갱신)
    counters:        Counters,                      // bytes_sent_today(+date), last_upload_at (Uploader가 갱신, DB에도 기록)
    uploader:        UploaderState,
    tasks:           HashMap<String, TaskStatus>,
    last_error:      Option<ErrorInfo>,
    equip_id_changed_from: Option<String>,          // 하트비트에 1회 실림
}
```

publish는 뮤텍스 안에서 스냅샷을 만들고 밖에서 `send`와 `on_change`를 호출한다.
polling 없이 이벤트가 있을 때만 갱신되므로 유휴 시 비용이 없다.

### 2.4 시간 규칙

| 용도 | 규칙 |
|---|---|
| 조각 날짜 디렉토리 `{YYYYMMDD}` | **조각 생성 시각**의 로컬 날짜 [설계 결정] |
| `bytes_sent_today`의 "오늘" | 로컬 날짜. 날짜가 바뀌면 0으로 리셋 |
| JSON 타임스탬프 | RFC 3339, 로컬 오프셋 포함 (`2026-09-10T14:03:00+09:00`) |
| 타이머 | `tokio::time::Instant` (단조). 벽시계가 필요한 스케줄은 매번 `Local::now()`에서 재계산 |
| DB 타임스탬프 | RFC 3339 문자열 |

---

## 3. 설정 (config 모듈)

### 3.1 타입

```rust
#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct Config {
    pub equip:  EquipConfig,   // num, name, id_separator
    pub agent:  AgentConfig,   // debounce_sec, max_wait_sec, spool_dir, spool_max_bytes,
                               // heartbeat_interval_sec, autostart, max_chunk_bytes [설계 결정, 기본 8 MiB]
    pub server: ServerConfig,  // unc_base, retry_max, retry_base_ms
    pub ui:     UiConfig,      // exit_confirm_message
    pub watch:  Vec<WatchConfig>,
    pub task:   Vec<TaskConfig>,
    pub log:    LogConfig,     // level, dir, max_files
}

pub struct WatchConfig {
    pub pattern:   String,
    pub header:    HeaderMode,          // auto | none | fixed
    pub columns:   Option<Vec<String>>, // fixed일 때 필수
    pub delimiter: String,              // 기본 ","  (1문자 또는 "\t")
    pub enabled:   bool,                // 기본 true [설계 결정] — 탭 3의 "활성화" 토글 대응
}

pub struct TaskConfig {
    pub name: String, pub program: String, pub args: Vec<String>, pub working_dir: Option<String>,
    pub schedule: ScheduleKind,         // hourly | every_n_min | daily
    pub interval_min: Option<u32>,      // every_n_min일 때 필수 [설계 결정]
    pub daily_at: Option<String>,       // daily일 때 필수, "HH:MM" [설계 결정]
    pub timeout_sec: u64, pub run_on_start: bool, pub enabled: bool,
}
```

모든 필드는 `#[serde(default)]`로 기본값을 가진다. 사양 3장의 TOML 예시가 그대로 파싱된다.

### 3.2 로드 시퀀스

```
main
 ├─ AppPaths::resolve()            C:\ProgramData\csv-agent\{config.toml, state.db, spool, logs}
 ├─ config::load(path)
 │    ├─ 파일 없음        → Config::default() 를 저장하고 사용 (첫 실행)
 │    ├─ 파싱 실패        → ConfigLoad::Broken{err}  (덮어쓰지 않음)
 │    └─ 성공             → validate() → (Config, Vec<ConfigError>)
 └─ 런타임 시작
      Broken이면 Config::default()로 런타임을 띄우되 config_errors에 파싱 오류를 넣는다.
      GUI가 열리고 사용자가 저장하면 그때 파일을 덮어쓴다 (덮어쓰기 전 config.toml.bak 생성).
```

### 3.3 검증 규칙

`validate()`는 오류 목록을 반환하며 각 항목은 `{field, message}`다. GUI는 필드 옆에 표시한다.

| 필드 | 규칙 |
|---|---|
| `equip.num`, `equip.name` | 앞뒤 공백 제거 후 비어 있으면 오류. 내부 공백은 `_`로 치환. `\ / : * ? " < > \|` 및 제어문자는 `_`로 치환. 치환이 일어나면 GUI에 치환 결과를 미리보기로 보여 준다 |
| `equip_id` 길이 | 결합 후 64자 초과 시 오류 |
| `equip.id_separator` | 비어 있거나 경로 불가 문자를 포함하면 오류 |
| `server.unc_base` | `\\host\share[\...]` 형식이 아니면 오류 (정규식 `^\\\\[^\\]+\\[^\\]+`). 존재 여부는 검사하지 않는다 (연결 테스트 버튼의 역할) |
| `agent.debounce_sec` | 1 이상. `max_wait_sec`는 `debounce_sec` 이상 |
| `agent.spool_max_bytes` | 16 MiB 이상 |
| `agent.max_chunk_bytes` | 64 KiB 이상 |
| `watch[].pattern` | 절대경로. glob 컴파일 성공. 고정 prefix 추출 가능 (`*`로 시작하는 드라이브 등 불가) |
| `watch[].columns` | `fixed`일 때 1개 이상, 각 항목 비어 있지 않음, 구분자 문자 포함 불가 |
| `watch[].delimiter` | 1문자 또는 `\t` |
| `task[].name` | 비어 있지 않고 서로 유일 |
| `task[].program` | 비어 있지 않음 |
| `task[]` 스케줄 | `every_n_min`이면 `interval_min` 1..=1440, `daily`면 `daily_at`이 `HH:MM` |
| `task[].timeout_sec` | 1 이상 |
| `log.level` | trace/debug/info/warn/error |

**equip 오류의 효과**: `equip_id`가 유효하지 않으면 Uploader와 Heartbeat가 `Gated` 상태로
대기한다. 감시·조각 생성·스풀 저장은 계속된다 (스풀 가드 한도까지). [설계 결정 D-08]

### 3.4 저장

GUI 저장 → `config::save(path, &config)`:

1. `toml::to_string_pretty` 로 직렬화
2. `config.toml.tmp` 에 쓰고 `sync_all`
3. `rename` 으로 교체

`toml` 크레이트는 주석을 보존하지 않는다. 설정은 GUI로 편집하는 것을 전제로 하고, 손편집용
주석은 배포 시 함께 두는 `config.example.toml`에 남긴다. [설계 결정 D-14]

### 3.5 런타임 반영 (즉시 적용)

`Command::ReloadConfig(new)` 를 받은 런타임은 `config_tx.send(Arc::new(new))` 로 방송한다.
각 워커는 자기 관심 항목이 바뀌었을 때만 반응한다.

| 항목 | 반응하는 워커 | 동작 |
|---|---|---|
| `equip.*` | SharedState, Uploader, Heartbeat | `equip_id` 재계산. 값이 바뀌면 `warn` 로그 + `equip_id_changed_from` 기록. Gated였다면 해제. 스풀에 남은 조각은 새 `equip_id` 디렉토리로 전송된다 (D-04) |
| `server.*` | Uploader, Heartbeat | 다음 시도부터 새 값 사용. Suspended였다면 즉시 재시도 |
| `agent.debounce_sec`, `max_wait_sec`, `max_chunk_bytes` | Debouncer, Chunker | 다음 타이머/다음 처리부터 적용 |
| `agent.heartbeat_interval_sec` | Heartbeat | 다음 발송 시각 재계산 |
| `agent.autostart` | 런타임 (win::registry) | 즉시 레지스트리 등록/해제 |
| `agent.spool_max_bytes` | Spool | 다음 조각 추가 시 가드 적용 |
| `watch[]` | Watcher, Debouncer | 패턴 집합이 바뀌면 notify 감시를 재구성하고 새 패턴에 초기 스캔. 더 이상 매칭되지 않는 파일의 대기 타이머는 취소 |
| `task[]` | Scheduler | 이름 기준 diff. 루프 재생성. 실행 중인 자식 프로세스는 완료까지 유지 |
| `ui.*` | app | GUI가 직접 읽음 |
| `log.level` | logging | `reload::Handle`로 즉시 |
| `agent.spool_dir`, `log.dir`, `log.max_files` | — | **재시작 후 적용**. GUI 해당 필드 옆에 표시. 스풀 디렉토리를 도중에 바꾸면 미전송 조각이 두 곳에 흩어지므로 제외 [설계 결정 D-13] |

`Config`는 `PartialEq`라 항목별 비교는 부분 구조체 비교로 충분하다.

---

## 4. 상태 저장 (state 모듈)

### 4.1 스키마

사양 4장의 두 테이블에 아래를 더한다. [설계 결정 D-10, D-11]

```sql
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);   -- schema_version, last_equip_id

CREATE TABLE progress (
  file_key     TEXT PRIMARY KEY,
  path         TEXT NOT NULL,        -- 정규화된 경로 (소문자, '/')
  display_path TEXT NOT NULL,        -- 원본 표기 (GUI용)
  name_seg     TEXT NOT NULL,        -- 조각 파일명의 {원본파일명} 세그먼트 (4.4 규칙으로 계산, 고정)
  offset       INTEGER NOT NULL,
  size         INTEGER NOT NULL,
  header_mode  TEXT NOT NULL,
  header       TEXT,
  header_kind  TEXT,
  field_count  INTEGER,
  last_seen    TEXT NOT NULL,
  updated_at   TEXT NOT NULL,
  last_error   TEXT                  -- 최근 처리 실패 사유 (GUI 표시용, 성공 시 NULL)
);
CREATE INDEX progress_path ON progress(path);

CREATE TABLE task_history (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  task_name   TEXT NOT NULL,
  started_at  TEXT NOT NULL,
  finished_at TEXT,
  exit_code   INTEGER,
  timed_out   INTEGER DEFAULT 0,
  skipped     INTEGER DEFAULT 0,     -- 이전 실행 미완료로 건너뜀
  duration_ms INTEGER,
  stdout_tail TEXT,                  -- 마지막 16 KiB (전체는 로그 파일에)
  stderr_tail TEXT
);
CREATE INDEX task_history_name ON task_history(task_name, id DESC);

CREATE TABLE counters (key TEXT PRIMARY KEY, value TEXT NOT NULL);
-- bytes_sent_date = "20260910", bytes_sent_today = "4823910", last_upload_at = "..."
```

- `PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;`
  offset 커밋이 크래시로 유실되면 같은 구간을 재전송할 뿐이므로 FULL은 불필요하다 [spec A.8].
- `task_history`는 작업별 최근 100건만 남기고 삽입 시 정리한다.
- `schema_version`으로 마이그레이션한다. 버전 불일치 시 순차 ALTER.

### 4.2 접근 모델

```rust
pub struct StateDb { conn: Mutex<rusqlite::Connection> }   // 단일 커넥션
```

호출은 항상 `spawn_blocking` 안에서 한다. 처리량이 낮아 커넥션 풀은 두지 않는다.
주요 메서드:

| 메서드 | 사용처 |
|---|---|
| `load_all_progress() -> Vec<ProgressRow>` | 시작 시 SharedState 적재 |
| `get_progress(file_key)` / `upsert_progress(row)` | Chunker |
| `commit_offset(file_key, offset, size, header*, updated_at)` | Chunker (한 트랜잭션) |
| `delete_progress_by_path(path)` | 삭제 이벤트 |
| `reset_offset(file_key)` | GUI 리셋 |
| `insert_task_run / finish_task_run / recent_task_runs(name, n)` | Scheduler, GUI |
| `get_counter / set_counter` | Uploader, 시작 시 |
| `get_meta / set_meta` | equip_id 변경 감지 |

### 4.3 손상 대응

열기 실패 또는 `PRAGMA integrity_check` 실패 시 `state.db` 를 `state.db.corrupt-{ts}` 로
옮기고 새로 만든다. 결과는 모든 파일이 offset 0에서 재업로드되는 것이며 서버 DEDUP이 흡수한다.
`error` 로그 + `last_error` 에 남긴다. [설계 결정 D-16]

---

## 5. 경로·이름 규칙 (paths 모듈)

### 5.1 경로 정규화

```rust
pub struct NormPath(String);   // 절대경로, '\\' → '/', 소문자, 끝 '/' 제거, "\\?\" 접두사 제거
pub fn normalize(p: &Path) -> NormPath;   // std::path::absolute 사용. canonicalize는 쓰지 않는다 ("\\?\" 회피)
```

DB `path` 컬럼, 디바운서 맵 키, 패턴 매칭은 모두 `NormPath` 기준이다.

### 5.2 glob 고정 prefix

```rust
pub struct CompiledPattern {
    pub source:  String,           // 원문
    pub prefix:  PathBuf,          // 고정 디렉토리 (glob 메타문자 `* ? [ {` 가 나오기 전까지의 컴포넌트)
    pub glob:    glob::Pattern,    // 정규화된 패턴
    pub opts:    glob::MatchOptions, // case_sensitive=false, require_literal_separator=true
    pub cfg:     WatchConfig,
}
```

- `D:/logs/**/vib_*.csv` → prefix `D:/logs`
- `C:/app/output/press_*.csv` → prefix `C:/app/output`
- `require_literal_separator=true` 이므로 `*` 는 디렉토리를 넘지 않고 `**` 만 넘는다.
- 여러 패턴의 prefix 중 조상 관계가 있으면 조상만 notify에 등록한다 (중복 이벤트 방지).

### 5.3 파일 고유 ID (win 모듈)

```rust
pub struct FileKey(String);   // "{volume_serial:08x}:{file_index:016x}"
pub fn file_key(file: &File) -> io::Result<FileKey>;   // GetFileInformationByHandle
```

파일은 `FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE` 로 연다. std의
`OpenOptions`가 Windows에서 기본으로 이 공유 모드를 쓰므로 별도 플래그는 필요 없다.
쓰기 프로그램이 공유를 거부하면 `ERROR_SHARING_VIOLATION`이 나며 이는 사양 7.1-4의 사전 확인 항목이다.

### 5.4 조각 파일명

```
{name_seg}__{start}-{end}__{hdr|syn}.csv.gz
```

`{name_seg}` 규칙 [설계 결정 D-05, 사양 변경 제안]:

1. 파일 경로에서 매칭된 패턴의 `prefix`를 뗀 **상대 경로**를 취한다.
2. 경로 구분자를 `~` 로 치환한다.
3. 경로 불가 문자와 제어문자를 `_` 로 치환한다.

```
패턴 D:/logs/**/vib_*.csv,  파일 D:/logs/vib_001.csv         → vib_001.csv          (사양과 동일)
패턴 D:/logs/**/vib_*.csv,  파일 D:/logs/lineA/vib_001.csv   → lineA~vib_001.csv
```

이렇게 해야 `**` 패턴에서 서로 다른 하위 디렉토리의 동명 파일이 서버에서 하나로 섞이지 않는다.
prefix 바로 아래 파일은 사양 규칙과 결과가 같고, 서버 파싱(오른쪽부터 `__` 두 번)도 그대로다.
`name_seg`는 파일을 처음 볼 때 계산해 DB에 고정한다. 이후 패턴 편집으로 prefix가 달라져도
같은 파일의 조각 이름이 흔들리지 않는다.

파싱 함수도 함께 둔다 (테스트 및 cli 도구용):

```rust
pub struct ChunkName { pub name_seg: String, pub start: u64, pub end: u64, pub kind: HeaderKind }
pub fn format_chunk_name(&ChunkName) -> String;
pub fn parse_chunk_name(&str) -> Option<ChunkName>;
```

---

## 6. 워커 설계

### 6.1 Watcher (watch 모듈)

**책임**: notify 감시 등록, 이벤트를 패턴으로 필터해 Debouncer에 전달, 초기 스캔, rescan.

```rust
pub struct Watcher {
    patterns:  Vec<CompiledPattern>,          // enabled == true 만
    notify:    Option<RecommendedWatcher>,    // ReadDirectoryChangesW
    roots:     Vec<PathBuf>,                  // 실제 등록된 디렉토리
    tx:        mpsc::UnboundedSender<FsEvent>,
    rescan:    Arc<Notify>,                   // 콜백이 notify_one, Watcher 태스크가 대기
    shared:    Arc<SharedState>,
}
pub enum FsEvent { Changed(PathBuf), Removed(PathBuf) }
```

**감시 등록 시퀀스**

```
build(patterns)
  ├─ prefix 집합 계산, 조상 중복 제거
  ├─ RecommendedWatcher::new(callback, notify::Config::default())
  ├─ 각 root: watcher.watch(root, Recursive)
  │     실패(디렉토리 없음 등) → watch_errors에 {pattern, reason} 기록, 계속 진행
  └─ initial_scan()
```

**콜백 (notify 스레드에서 실행)**

```
event
  ├─ need_rescan()            → rescan.notify_one()   (Watcher 태스크가 initial_scan 재실행)
  ├─ kind == Remove(*)        → 경로별 tx.send(Removed)
  ├─ kind == Modify(Name(From)) → Removed(old)      (이름 변경의 원본 = 삭제 취급 [spec 2.9])
  ├─ kind == Create | Modify(Data|Size|Any|Name(To|Any)) → 패턴 매칭 성공 시 Changed(path)
  └─ 그 외(Access, Metadata)  → 무시
```

패턴 매칭을 콜백에서 하는 이유: 감시 루트에 관심 없는 파일이 많을 때 tokio 워커를 깨우지 않기
위해서다. 매칭은 문자열 비교뿐이라 콜백 스레드에서 충분히 가볍다.

**초기 스캔** [설계 결정 D-01]: 시작 시와 `watch[]` 변경 시, 각 root를 `walkdir`로 한 번
훑어 매칭 파일마다 `Changed(path)`를 보낸다. 에이전트가 꺼져 있던 동안 자란 파일이 이벤트 없이
누락되는 것을 막는다. 스캔 중 발견되지 않은 DB 행은 지우지 않고 `present=false`로만 표시한다
(다른 드라이브가 일시적으로 분리된 경우 등을 고려). 스캔은 `spawn_blocking`에서 1회 수행한다.

**rescan** [spec 7.2]: `ReadDirectoryChangesW` 버퍼 오버플로 시 notify가 `Rescan` 플래그를
붙인 이벤트를 준다. 이때 초기 스캔과 같은 절차를 다시 돈다. 주기적 전체 스캔은 두지 않는다.

**감시 실패 재시도**: `watch_errors`가 비어 있지 않으면 Heartbeat의 nudge(기본 5분)를 받을
때만 `build()`를 다시 시도한다. 별도 타이머를 두지 않아 유휴 부하가 늘지 않는다. [설계 결정 D-02]

### 6.2 Debouncer (debounce 모듈)

**책임**: 파일별 타이머 재설정형 디바운스 + `max_wait_sec` 강제 처리 [spec 2.3, 2.3.1].

```rust
pub struct Debouncer {
    pending:   HashMap<NormPath, Pending>,
    rx:        mpsc::UnboundedReceiver<FsEvent>,
    out:       mpsc::Sender<ProcessRequest>,
    fired_tx:  mpsc::UnboundedSender<NormPath>,     // 타이머 태스크 → 본체: 맵 정리용
    fired_rx:  mpsc::UnboundedReceiver<NormPath>,
    config_rx: watch::Receiver<Arc<Config>>,
    cancel:    CancellationToken,
}
struct Pending { handle: JoinHandle<()>, first_event_at: Instant, deadline: Instant, reason: Reason }
```

**본체 루프**

```
loop select! {
  Some(ev) = rx.recv()        => on_event(ev)
  Some(p)  = fired_rx.recv()  => pending.remove(p)
  _ = config_rx.changed()     => 다음 타이머부터 새 debounce/max_wait 사용 (진행 중 타이머는 유지)
  _ = cancel.cancelled()      => 모든 handle.abort(); return
}
```

**on_event(Changed(path))**

```
now = Instant::now()
first = pending.get(path).map(|p| p.first_event_at).unwrap_or(now)
deadline = min(now + debounce_sec, first + max_wait_sec)
if let Some(old) = pending.remove(path) { old.handle.abort() }
handle = spawn(async move {
    sleep_until(deadline).await;                 // 커널 타이머 대기. 실행 큐에서 빠짐
    out.send(ProcessRequest{path, reason}).await;
    fired_tx.send(path);
})
pending.insert(path, Pending{handle, first_event_at: first, deadline, reason})
```

- `deadline`이 `first + max_wait`에 의해 결정되면 `reason = MaxWait`, 아니면 `Quiet`.
- `Removed(path)`: pending 취소 후 Chunker에 `Removed`를 그대로 전달.
- rescan은 Watcher 내부에서 초기 스캔을 다시 돌리는 것으로 끝난다. 스캔 결과는 평소처럼 `Changed`로 들어오므로 Debouncer는 관여하지 않는다.
- 처리 중인 파일에 새 이벤트가 오면 다시 스케줄된다. Chunker는 offset 기준으로 멱등하므로 안전하다.

**대안 메모**: `tokio_util::time::DelayQueue` 하나로 모든 파일의 타이머를 관리해도 동일한
CPU 특성을 갖는다. 사양 2.3.1이 `JoinHandle` + `abort()`를 명시하고 있어 그대로 따른다.

### 6.3 Chunker (chunk 모듈)

**책임**: 증분 읽기, 개행 절단, 헤더 준비, gzip, 스풀 저장, offset 커밋 [spec 2.4, 2.5, 2.6].
단일 태스크가 요청을 순차 처리한다. 실제 작업은 전부 동기 IO이므로 요청 하나를 통째로
`spawn_blocking` 에 넘긴다.

```rust
pub struct Chunker { rx: mpsc::Receiver<ProcessRequest>, ctx: Arc<ChunkCtx> }
pub struct ChunkCtx { db: Arc<StateDb>, spool: Arc<Spool>, shared: Arc<SharedState>,
                      config_rx: watch::Receiver<Arc<Config>>, upload_notify: Arc<Notify> }

pub fn process_file(ctx: &ChunkCtx, path: &Path, reason: Reason) -> Result<Outcome, ChunkError>;
pub enum Outcome { NoChange, NoNewline, Chunked { chunks: u32, bytes: u64 }, Skipped(String) }
```

**process_file 시퀀스**

```
 1. 패턴 매칭: config.watch 순서대로 첫 일치 → WatchConfig. 없으면 Skipped (패턴이 바뀐 경우)
 2. File::open(path)                      실패 → ChunkError::Open (공유 위반 포함). 5분당 1회로 로그 억제
 3. file_key = win::file_key(&file)
 4. row = db.get_progress(file_key) 또는 신규 {offset 0, size 0, header None, name_seg 계산}
    - row.header_mode != cfg.header 이면 (설정 변경) header/header_kind/field_count = None, mode 갱신
 5. size = metadata.len()
    - size < row.offset  → truncate/재생성으로 간주: offset 0, header None, warn 로그
    - size == row.offset → last_seen 갱신, NoChange
 6. 헤더 준비 (6.3.1)  → 실패 시(헤더 줄 미완성 등) 이번 사이클 종료
 7. loop:                                     ← max_chunk_bytes 단위로 반복 [설계 결정 D-06]
      read_len = min(size - offset, max_chunk_bytes)
      seek(offset), read_exact(read_len)  → buf
      cut = buf.rfind(b'\n')  없으면 → NoNewline, break (커밋하지 않음)
      end = offset + cut + 1
      body = &buf[..=cut]
      spool.write_chunk(ChunkSpec{name_seg, start: offset, end, kind, header, body}, spool_max_bytes)   (6.4)
      db.commit_offset(file_key, end, size, header, kind, field_count)     ← 저장 완료 후 커밋
      shared.update(files[file_key] = ...)
      offset = end
      if end == size { break }          // 남은 바이트가 있으면 다음 반복. 개행이 없으면 NoNewline으로 자연 종료
 8. upload_notify.notify_one()
```

7단계에서 `body`가 `max_chunk_bytes`에 걸려 잘리면 마지막 개행 앞까지만 조각이 되고,
남은 바이트는 다음 반복에서 이어진다. 초기 전량 업로드 시 조각이 여러 개로 나뉘어 스풀과
SMB 전송 단위가 작아진다.

#### 6.3.1 헤더 준비

| 모드 | 절차 | `header_kind` |
|---|---|---|
| `auto` | 파일 offset 0에서 첫 `\n`까지 읽는다 (상한 64 KiB). 개행이 없으면 "헤더 미완성"으로 이번 사이클 종료. 줄 끝 `\r\n`/`\n` 제거한 문자열을 `header`로 저장. 저장된 값과 다르면 `warn` 로그 후 갱신 [spec 2.5 헤더 변경 감지] | `hdr` |
| `none` | offset 위치의 첫 데이터 줄에서 필드 수를 센다 (따옴표 인식: `"` 안의 구분자는 세지 않음). 저장된 `field_count`와 다르면 `warn` 후 `c0..c{n-1}`을 구분자로 이어 다시 만든다 | `syn` |
| `fixed` | `columns.join(delimiter)` | `syn` |

**`auto` 모드에서 offset 0 조각의 규칙** [설계 결정 D-07]: 원본의 첫 줄이 이미 헤더이므로
`start == 0`인 조각은 헤더를 앞에 붙이지 않고 원본 바이트를 그대로 담는다. 조각 내용의 첫 줄이
헤더라는 사양 규칙과, 파일명 offset이 원본 바이트 위치와 1:1로 대응한다는 규칙을 동시에 만족한다.
`start > 0`인 조각은 `header + "\n" + body`다.

같은 이유로 `auto` 모드에서 `start == 0`이고 본문이 헤더 줄로 끝나면 (데이터 행이 아직 없음)
조각을 만들지 않고 종료한다. 헤더만 담긴 빈 조각을 서버에 보내지 않기 위해서다. 헤더는 이미
DB에 저장되어 있으므로 데이터가 들어오면 그때 `0-{end}` 조각이 헤더와 데이터를 함께 담는다.

`none`/`fixed` 모드는 `start`와 무관하게 항상 `header + "\n" + body`다.

#### 6.3.2 삭제 처리

`Removed(path)` 수신 시 `db.delete_progress_by_path(normalize(path))` 후 SharedState에서 제거.
파일이 이미 없으므로 `file_key`를 다시 얻을 수 없어 경로 컬럼으로 찾는다.

#### 6.3.3 offset 리셋

`Command::ResetOffset(file_key)` → `db.reset_offset` (offset 0, header NULL, field_count NULL)
→ 해당 경로에 `ProcessRequest{reason: Manual}` 을 넣는다.

### 6.4 Spool (spool 모듈)

**레이아웃** [설계 결정 D-03]

```
{spool_dir}/
  20260910/
    vib_001.csv__4096-8192__hdr.csv.gz
    press_a.csv__0-2048__syn.csv.gz.tmp     ← 쓰는 중. Uploader는 .tmp 를 무시
  20260911/
```

날짜 디렉토리가 곧 업로드 대상의 `{YYYYMMDD}` 다. `equip_id`는 스풀에 담지 않고 전송 시점에
결정한다 (D-04).

```rust
pub struct Spool { dir: PathBuf, stats: Mutex<SpoolStats>, shared: Arc<SharedState> }
pub struct SpoolStats { pending: u64, bytes: u64 }

impl Spool {
    pub fn init(&self)                       // 시작 시: 남은 .tmp 삭제, 총량/건수 집계
    pub fn write_chunk(&self, spec, max_bytes: u64) -> io::Result<PathBuf>   // gzip → .tmp → sync_all → rename → guard(max_bytes)
    pub fn list_oldest_first(&self) -> Vec<SpoolItem>        // {path, date_dir, name, size, mtime}
    pub fn remove(&self, item)               // 전송 완료 후. 빈 날짜 디렉토리도 제거
}
```

**write_chunk**: `GzEncoder<File>` 로 `.tmp`에 헤더와 본문을 순서대로 쓴다 → `finish()` →
`sync_all()` → `rename`. rename 전에는 Uploader가 보지 못하므로 부분 파일이 전송될 수 없다.

**디스크 가드** [spec 2.7]: 추가 후 `bytes > spool_max_bytes` 인 동안 가장 오래된 항목(mtime)
부터 삭제하며 `warn` 로그에 조각 이름(offset 구간 포함)을 남긴다. 삭제된 구간은 서버에서
결번으로 보인다. 삭제는 방금 쓴 조각 자신에도 적용될 수 있다.

### 6.5 Uploader (upload 모듈)

**책임**: 스풀 → UNC 전송, 원자적 배치, 백오프, 게이팅 [spec 2.8].

```rust
pub struct Uploader { notify: Arc<Notify>, spool: Arc<Spool>, db: Arc<StateDb>, shared: Arc<SharedState>,
                      config_rx: watch::Receiver<Arc<Config>>, cancel: CancellationToken,
                      state: UploaderState, consecutive_failures: u32 }
pub enum UploaderState { Gated, Idle, Draining, Backoff { until: Instant, attempt: u32 }, Suspended }
```

**상태 머신**

```
            equip_id 무효 / unc_base 무효
  Gated  ◀──────────────────────────────┐
    │ 설정 변경으로 유효화                │
    ▼                                   │
  Idle ──Notify/FlushNow/nudge──▶ Draining ──성공적으로 비움──▶ Idle
                                    │ 실패
                                    ▼
                               Backoff{attempt}  ──sleep_until 만료──▶ Draining
                                    │ attempt > retry_max
                                    ▼
                               Suspended ──Notify/FlushNow/nudge/설정 변경/SystemResumed──▶ Draining
```

- 백오프 지연: `retry_base_ms * 2^(attempt-1)`, 상한 5분. `retry_max` 초과 시 Suspended.
- Suspended는 스스로 깨어나지 않는다. 깨우는 신호는 새 조각, 수동 전송, Heartbeat 성공(nudge),
  설정 변경, 절전 복귀뿐이다. 이 중 Heartbeat nudge가 "네트워크 복구 후 자동 재개"를 담당한다.
  [설계 결정 D-09]
- 전송 성공은 `consecutive_failures = 0`.

**drain (spawn_blocking)**

```
for item in spool.list_oldest_first():
    target_dir = {unc_base}\raw\{equip_id}\{item.date_dir}
    create_dir_all(target_dir)
    copy(item.path, target_dir\{name}.tmp)            // 실패 시 .tmp 삭제 시도 후 Err
    rename(target_dir\{name}.tmp, target_dir\{name})   // 기존 파일이 있으면 교체 (재전송 케이스)
    spool.remove(item)
    counters.bytes_sent_today += item.size (날짜 넘어가면 리셋), last_upload_at = now
    db.set_counter(...)  shared.update(...)
    if cancel.is_cancelled() { return Ok(Interrupted) }
return Ok(Empty)
```

한 파일이라도 실패하면 drain을 중단하고 Err로 돌아온다 (같은 원인이 반복될 가능성이 높으므로).
UNC IO는 네트워크 타임아웃으로 수십 초 블록될 수 있어 반드시 `spawn_blocking`에서 실행한다.

### 6.6 Scheduler / Runner (task 모듈)

**책임**: 주기 명령 실행 [spec 2.10].

```rust
pub struct Scheduler { loops: HashMap<String, JoinHandle<()>>, running: Arc<Mutex<HashMap<String, RunningTask>>>, ... }

pub enum ScheduleKind { Hourly, EveryNMin, Daily }
pub fn next_run(cfg: &TaskConfig, now: DateTime<Local>) -> DateTime<Local>;
```

**next_run 규칙** [설계 결정 D-12]

| schedule | 다음 실행 |
|---|---|
| `hourly` | 다음 정각 (`now`가 정각이면 그 다음 정각) |
| `every_n_min` | 자정 기준 `interval_min` 배수 중 `now` 이후 첫 시각 (n=15 → :00 :15 :30 :45) |
| `daily` | 오늘 `daily_at`, 지났으면 내일 |

**작업 루프 (task마다 하나)**

```
if run_on_start { run_once(reason=Startup) }
loop {
    next = next_run(cfg, Local::now())
    wait = (next - Local::now()).min(1h)               ← 절전 복귀 대비 최대 1시간 단위로 끊어 대기
    select! {
        _ = sleep(wait)             => if Local::now() >= next { run_once(Scheduled) }   // 아직이면 재계산
        _ = resume_notify.notified() => continue        // SystemResumed → 즉시 재계산
        _ = cancel.cancelled()       => return
    }
}
```

1시간 상한은 절전 복귀나 시계 변경으로 `sleep`이 실제 시각과 어긋났을 때 최대 1시간 안에
바로잡기 위한 것이다. 한 시간에 한 번 깨는 것은 유휴 CPU 0% 기준에 영향을 주지 않는다.
절전 복귀 알림이 오면 즉시 재계산하므로 실질적인 지연은 없다.

**run_once**

```
if running.contains(name) { db.insert(skipped=1); warn 로그; return }
run_id = db.insert_task_run(name, started_at)
child = tokio::process::Command::new(program).args(args).current_dir(working_dir)
          .creation_flags(CREATE_NO_WINDOW).stdin(null).stdout(piped).stderr(piped)
          .kill_on_drop(true).spawn()
win::job::assign(child)                                  // Job Object, KILL_ON_JOB_CLOSE → 자식 트리까지 종료 [설계 결정 D-15]
running.insert(name, RunningTask{ started, job })
(out, err) = 동시에 읽기 (각 최대 1 MiB, 초과분은 버리고 표시)
select! {
    status = child.wait()            => exit_code
    _ = sleep(timeout_sec)           => job.terminate(); timed_out = true
}
db.finish_task_run(run_id, exit_code, timed_out, duration, stdout_tail, stderr_tail)
info/warn 로그: 종료 코드, 소요 시간, stdout/stderr 전체
shared.update(tasks[name].last = ...)
running.remove(name)
```

- `Command::RunTaskNow(name)` 은 `run_once(Manual)` 을 spawn 한다. 실행 중이면 건너뛰기 규칙이 같이 적용된다.
- 설정 변경 시 루프만 재생성한다. `running` 맵은 루프 밖에 있어 실행 중인 자식은 계속 돈다.

### 6.7 Heartbeat (heartbeat 모듈)

```rust
pub struct Heartbeat { shared, config_rx, cancel, uploader_nudge: Arc<Notify>, watcher_nudge: Arc<Notify> }
```

```
loop {
    if equip_id/unc_base 무효 { config_rx.changed().await; continue }      // Gated
    select! {
        _ = sleep_until(next)          => {}
        _ = config_rx.changed()        => { next 재계산; continue }
        _ = resume_notify.notified()   => {}
        _ = cancel.cancelled()         => return
    }
    result = spawn_blocking(write_status)      // _status\{equip_id}.json.tmp → rename
    if ok  { uploader_nudge.notify_one(); if watch_errors 있음 { watcher_nudge.notify_one() } }
    else   { warn 로그 (10분당 1회 억제) }
    next = now + heartbeat_interval
}
```

시작 직후 1회 즉시 보내고 이후 주기대로 보낸다. JSON은 사양 2.11 필드에 아래를 더한다.

| 필드 | 내용 |
|---|---|
| `agent_started_at` | 프로세스 시작 시각 |
| `equip_id_changed_from` | 마지막 변경 전 값. 변경 후 첫 하트비트에만 실리고 이후 `null` |
| `config_errors` | 문자열 배열 |
| `watch_errors` | `[{pattern, reason}]` |
| `uploader_state` | `idle` / `draining` / `backoff` / `suspended` / `gated` |

---

## 7. 런타임 (runtime 모듈)

```rust
pub struct AgentRuntime { thread: JoinHandle<()>, handle: AgentHandle }

pub fn start(config, paths, on_change) -> Result<AgentHandle, StartError>
```

**시작 시퀀스**

```
 1. StateDb::open(paths.state)         무결성 검사, 마이그레이션 (4.3)
 2. Spool::init()                      .tmp 정리, 통계
 3. SharedState::new(config, db.load_all_progress(), counters)
    - meta.last_equip_id != 현재 equip_id 이면 warn + equip_id_changed_from
 4. 채널 생성: cmd(mpsc), config(watch), status(watch), fs(mpsc unbounded), process(mpsc 1024),
    upload_notify, watcher_nudge, resume_notify, cancel token
 5. std::thread "agent-runtime" 에서 tokio 런타임 build → block_on(run(...))
 6. run(): Watcher::build → Debouncer / Chunker / Uploader / Scheduler / Heartbeat spawn
           → 명령 루프 (아래)
```

**명령 루프**

```
loop select! {
    Some(cmd) = cmd_rx.recv() => match cmd {
        ReloadConfig(c)      => config_tx.send(c); autostart 레지스트리 반영; watcher.reconfigure(); scheduler.reconfigure()
        FlushNow             => upload_notify.notify_one()
        ProcessFileNow(p)    => process_tx.send(Manual)
        ResetOffset(k)       => chunker 경로
        RunTaskNow(n)        => scheduler.run_now(n)
        TestUnc{base,reply}  => spawn_blocking(probe)   // {base}\_status\.probe-{host}-{pid}-{ts}.tmp 쓰고 삭제
        PreviewPattern{..}   => spawn_blocking(scan, 상한 500건)
        TaskHistory{..}      => spawn_blocking(db.recent_task_runs)
        SystemResumed        => resume_notify.notify_waiters(); upload_notify.notify_one()
        Shutdown             => break
    }
}
cancel.cancel()
timeout(10s, join_all(workers))      // Chunker의 진행 중인 조각은 완료까지 기다린다
```

**Shutdown 후**: 런타임 스레드가 끝나면 GUI가 창을 닫고 프로세스가 종료된다. Uploader가 rename
직후 로컬 삭제 전에 끊기면 다음 시작 때 같은 조각이 다시 올라가 서버 파일을 교체한다. 내용이
같으므로 무해하다.

---

## 8. 트레이와 GUI (app 크레이트)

### 8.1 이벤트 루프 통합

eframe(winit)이 메인 스레드 메시지 루프를 소유한다. tray-icon은 메시지 루프가 있는 스레드에서
만들어야 하므로 `eframe::run_native`의 앱 생성 클로저 안에서 `TrayIcon`을 만든다. winit이
Windows 메시지를 펌프하므로 트레이 아이콘의 WndProc도 함께 처리된다.

**트레이 이벤트 수신**: `TrayIconEvent::set_event_handler` 와 `MenuEvent::set_event_handler` 에
콜백을 등록한다. 콜백은 메시지를 펌프하는 메인 스레드에서 호출되며, `Mutex<VecDeque<UiEvent>>`에
넣고 `ctx.request_repaint()` 를 호출한다. `receiver().try_recv()` 를 주기적으로 확인하는 방식은
폴링이므로 쓰지 않는다.

**런타임 상태 수신**: `on_change` 훅이 `ctx.request_repaint()` 를 호출한다. `update()`에서
`status_rx.borrow()` 로 최신 스냅샷을 읽는다.

**창이 숨겨져 있을 때의 처리** [spec 5.2 렌더링 정지]

```
update(ctx):
    drain ui_events → 처리 (메뉴 선택, 더블클릭 등)
    snapshot = status_rx.borrow().clone()
    tray.sync(&snapshot)                 // 아이콘/툴팁이 바뀐 경우에만 set_icon/set_tooltip
    if !window_visible {
        return                           // UI를 구성하지 않는다. 다음 이벤트까지 잠든다
    }
    ... 탭 UI ...
```

`request_repaint_after` 나 애니메이션은 사용하지 않는다. 창이 숨겨진 상태에서 `update()`가 호출되는
계기는 트레이 이벤트와 런타임 상태 변경뿐이며, 파일 이벤트가 없는 유휴 상태에서는 하트비트마다
한 번(기본 5분)이다. 사양 2.3.1의 10분 방치 검증에서 두 번 깨어나는 정도이며 CPU 0% 표시에 영향이 없다.

**창 닫기(X)** → `close_requested()` 감지 시 `ViewportCommand::CancelClose` + `ViewportCommand::Visible(false)`.
**트레이 더블클릭 / "설정 열기" / 두 번째 인스턴스 신호** → `Visible(true)` + `Focus`.
**시작 시** 창은 숨긴 채 시작한다. 단, `config_errors`가 있으면 (첫 실행 포함) 창을 띄운다. [설계 결정 D-17]

### 8.2 트레이 (tray 모듈)

```rust
pub struct Tray { icon: TrayIcon, icons: [Icon; 3], current: TrayState, tooltip: String }
pub enum UiEvent { ShowWindow, FlushNow, OpenLogDir, ExitRequested, SecondInstance }
```

- 아이콘 3종은 `assets/icons/*.png` 를 `build.rs` 또는 `include_bytes!` + `image` 크레이트로
  디코드해 `Icon::from_rgba`로 만든다.
- 툴팁: `{equip_id 또는 "설정 필요"} · 마지막 전송 {HH:MM 또는 "없음"} · 대기 {n}건`.
- 메뉴: 설정 열기 / 지금 전송 / 로그 폴더 열기 / 구분선 / 종료. "로그 폴더 열기"는 `explorer.exe {log_dir}`.

### 8.3 종료 확인 [spec 5.4]

```
ExitRequested
  → 창을 보이게 한 뒤 모달 표시 (egui Modal). 포커스는 "취소"
  → 본문: 기본 문구 + (exit_confirm_message가 비어 있지 않으면 빈 줄 후 그대로 덧붙임, 줄바꿈 유지)
  → "종료": cmd_tx.send(Shutdown) → 런타임 스레드 join (최대 10초, 진행 표시) → ViewportCommand::Close
  → "취소" / Esc: 모달 닫기
```

Enter가 "취소"에 떨어지도록 기본 포커스를 명시한다. 트레이 메뉴 외에는 종료 경로가 없다.

### 8.4 이중 실행 방지 (single_instance 모듈) [spec 5.1]

```
main
  mutex = CreateMutexW(None, false, "Local\\csv-agent-{sha1(exe_path)[..8]}")
  if GetLastError() == ERROR_ALREADY_EXISTS:
      ev = OpenEventW("Local\\csv-agent-show-{...}"); SetEvent(ev); exit(0)
  else:
      ev = CreateEventW(auto-reset)
      std::thread: loop { WaitForSingleObject(ev, INFINITE); push UiEvent::SecondInstance; ctx.request_repaint() }
```

`Local\` 네임스페이스라 로그인 세션마다 독립이다. 대기 스레드는 이벤트 객체에서 블록되므로
CPU를 쓰지 않는다. `FindWindow` 기반 활성화는 창 제목 변경에 취약해 쓰지 않는다.

### 8.5 절전 복귀 (win::power)

`RegisterSuspendResumeNotification(DEVICE_NOTIFY_CALLBACK)` 으로 `PBT_APMRESUMEAUTOMATIC` 을 받아
`Command::SystemResumed` 를 보낸다. 창이 필요 없어 GUI와 독립적으로 동작한다. Scheduler·Heartbeat는
즉시 재계산하고 Uploader는 Suspended였다면 재시도한다.

### 8.6 자동 시작 (win::registry)

`HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 의 값 `CsvAgent` = `"{exe}" --hidden`.
`agent.autostart` 변경 시 즉시 등록/삭제한다. 시작 시 레지스트리 값과 설정이 어긋나면 설정을 기준으로
맞춘다 (exe 경로가 바뀐 경우 대응).

### 8.7 GUI 상태 모델 (ui 모듈)

```rust
pub struct App {
    handle:     AgentHandle,
    draft:      ConfigDraft,            // 편집 중 (숫자도 String). 저장 전까지 런타임에 영향 없음
    applied:    Arc<Config>,            // config_rx 최신본
    dirty:      bool,
    tab:        Tab,
    modal:      Option<Modal>,          // ExitConfirm | ResetOffsetConfirm(FileKey) | Error(String)
    pending:    PendingReplies,         // TestUnc / PreviewPattern / TaskHistory 의 oneshot Receiver
    window_visible: bool,
    tray:       Tray,
}
```

- **저장 흐름**: `draft.to_config()` → `validate()` → 오류면 필드 옆 표시 → 성공 시 `config::save`
  → `Command::ReloadConfig` → `dirty=false`. 저장 실패(권한 등)는 모달로 표시.
- oneshot 응답은 `update()`에서 `try_recv`로 확인한다. 런타임이 응답을 보낼 때 `on_change`를 호출하므로
  폴링 없이 다음 프레임에 표시된다.
- 탭 UI는 사양 5.5의 항목을 그대로 구현한다. 아래는 사양에 없는 세부만 적는다.

| 탭 | 세부 |
|---|---|
| 1 전역 설정 | `equip` 입력은 키 입력마다 `validate` 해 치환 결과와 최종 경로(`\\...\raw\{equip_id}\{오늘}\`)를 미리보기. 연결 테스트는 진행 중 버튼 비활성화, 결과는 성공/실패 + Win32 오류 메시지 한국어 매핑(권한 없음, 경로 없음, 네트워크 없음). `spool_dir`, `log.dir`, `log.max_files` 옆에 "재시작 후 적용" 표시 |
| 2 주기 명령 | 스케줄 편집은 콤보(`매시 정각` / `n분마다` + 숫자 / `매일` + HH:MM). 프로그램 경로는 `rfd` 파일 대화상자. 하단 이력은 선택 시 `Command::TaskHistory`로 최근 20건 조회, stdout/stderr tail은 모노스페이스 스크롤 영역 |
| 3 파일 감시 | 패턴 편집 시 "매칭 미리보기" 버튼 → `PreviewPattern` (상한 500건, 초과 시 "...외 n건"). `fixed` 선택 시 컬럼 입력란(쉼표 구분 텍스트)이 활성화. 현황 테이블은 `egui_extras::TableBuilder`, 진행률은 `offset/size`. offset 리셋은 확인 모달 후 `ResetOffset`. 전송 현황에 `uploader` 상태와 Backoff 남은 시간 표시 |

---

## 9. 유휴 CPU 0% 보증

사양 2.3.1의 검증 기준을 만족하기 위한 컴포넌트별 대기 방식과 깨어나는 조건이다.
구현 리뷰 시 이 표와 다르게 깨어나는 코드가 있는지 확인한다.

| 컴포넌트 | 대기 원어 | 깨어나는 조건 |
|---|---|---|
| notify 스레드 | `GetQueuedCompletionStatus` (ReadDirectoryChangesW 완료) | 감시 루트 아래 파일 변경 |
| Watcher rescan 태스크 | `Notify` / `config.changed()` / 하트비트 nudge | 버퍼 오버플로, 패턴 변경, 감시 실패 재시도 |
| Debouncer 본체 | `select!` on mpsc recv | 파일 이벤트, 타이머 만료 통지, 설정 변경 |
| Debouncer 타이머 태스크 | `sleep_until` | 만료 (파일당 1회) |
| Chunker | mpsc recv | 처리 요청 |
| Uploader | `Notify` / `sleep_until`(백오프) / `config.changed()` | 새 조각, 수동, nudge, 백오프 만료, 설정 |
| Scheduler 루프 | `sleep` (≤ 1h) | 다음 실행 시각, 절전 복귀, 설정 |
| Heartbeat | `sleep_until` | 주기(기본 5분), 절전 복귀, 설정 |
| 명령 루프 | mpsc recv | GUI 명령 |
| tokio 워커 스레드 | 런타임 파킹 (IOCP/condvar) | 위 태스크 중 하나가 준비됨 |
| tracing-appender 스레드 | 채널 recv | 로그 레코드 |
| single-instance 스레드 | `WaitForSingleObject(INFINITE)` | 두 번째 인스턴스 |
| 메인 스레드 (GUI) | `GetMessage` (winit `ControlFlow::Wait`) | OS 메시지, `request_repaint`, 트레이 이벤트 |

**금지 목록 (코드 리뷰 체크)**

- `notify::PollWatcher`, `notify::Config::with_poll_interval`
- `tokio::time::interval` 주기 1분 미만, `thread::sleep` 루프, `try_recv` 루프
- egui `request_repaint_after`, `ctx.request_repaint()`의 무조건 호출(매 프레임)
- `tray_icon::TrayIconEvent::receiver().try_recv()` 를 프레임마다 확인
- SQLite `busy_timeout` 대신 스핀 재시도
- `spawn_blocking` 안에서 sleep으로 기다리기

**검증 절차**: 감시 대상 변경 없이 10분 방치 → 작업 관리자 "세부 정보" 탭에서 CPU 0% 확인.
추가로 Windows Performance Recorder로 컨텍스트 스위치 횟수를 재면 하트비트 2회분만 나와야 한다.

---

## 10. 에러 처리와 복구 시나리오

### 10.1 로그 레벨 규칙

| 레벨 | 대상 |
|---|---|
| `error` | 상태 DB 손상, 설정 파일 파싱 실패, 감시 등록 실패, retry_max 초과 |
| `warn` | truncate 감지, 헤더 변경, 필드 수 변동, equip_id 변경, 스풀 초과 삭제, 전송 실패(재시도), 작업 타임아웃/건너뜀/비정상 종료, 하트비트 실패 |
| `info` | 조각 생성(파일·구간·헤더 종류·압축 후 크기), 전송 성공, 작업 실행 결과, 설정 반영, 시작/종료 |
| `debug` | 이벤트 수신, 디바운스 재설정, 개행 없음으로 대기 |

같은 원인이 반복될 때는 `RateLimited` 래퍼로 원인별 5~10분당 1회로 억제한다 (공유 위반, UNC 실패, 하트비트 실패).

### 10.2 시나리오별 동작

| 상황 | 감지 | 동작 | 결과 |
|---|---|---|---|
| 스풀 `.tmp` 쓰기 중 크래시 | 시작 시 `.tmp` 존재 | 삭제 | offset 미커밋이라 다음 처리에서 재생성 |
| 스풀 rename 후 offset 커밋 전 크래시 | — | 다음 처리에서 같은 구간을 다시 만듦 (이름 동일) | 스풀에 같은 이름이 있으면 덮어씀. 서버에 이미 갔다면 중복 → DEDUP |
| UNC rename 후 로컬 삭제 전 크래시 | — | 재전송, 서버 파일 교체 | 무해 |
| UNC `.tmp` 쓰다 끊김 | IO 오류 | `.tmp` 삭제 시도 후 백오프 | 서버 `.tmp` 잔여물은 다음 시도에서 덮어씀 |
| 네트워크 단절 / 서버 다운 | 전송 실패 반복 | Backoff → Suspended. 트레이 "오류". 하트비트 성공 시 자동 재개 | 스풀 축적, 가드 한도까지 보존 |
| 권한 없음 (ACCESS_DENIED) | 전송 실패 | 위와 동일. `last_error`에 원인 표시. 연결 테스트로 확인 유도 | |
| 스풀 상한 초과 | 추가 시 | 오래된 조각 삭제 + warn | 해당 구간 서버 결번 |
| 로컬 디스크 풀 | 스풀 쓰기 실패 | `ChunkError::Io`, offset 미커밋, 파일 `last_error` 표시 | 다음 이벤트에서 재시도 |
| 원본 파일 공유 위반 | open 실패 | warn(억제), `last_error` | 사전 확인 항목. 다음 이벤트에서 재시도 |
| 파일 truncate / 재생성(같은 file_key) | `size < offset` | offset 0, 헤더 재파생, warn | 전량 재업로드 → DEDUP |
| 파일 삭제 후 같은 이름으로 생성 | Removed → 새 file_key | 행 삭제 후 새 행 offset 0 | 전량 재업로드 → DEDUP |
| 파일 이름 변경 | Name(From)=Removed, Name(To)=Changed | 옛 행 삭제, 새 이름으로 offset 0 | 같은 데이터가 다른 name_seg로 재업로드 → DEDUP [spec 2.9] |
| 마지막 줄이 개행 없이 끝남 | `rfind('\n')` 실패 | 커밋 없이 종료 | 다음 이벤트에서 처리 |
| 헤더 줄이 아직 완성되지 않음 (auto) | 첫 64 KiB에 `\n` 없음 | 커밋 없이 종료 | |
| 헤더 변경 (auto) | 저장값 ≠ 현재 첫 줄 | warn, 갱신. 이후 조각은 새 헤더 | 서버가 조각 단위로 파싱하므로 일관 |
| 필드 수 변동 (none) | 첫 데이터 줄 필드 수 ≠ 저장값 | warn, 합성 헤더 재생성 | |
| 감시 디렉토리 부재 | `watch()` 실패 | `watch_errors`, 트레이 "오류", 하트비트 주기로 재시도 | |
| `ReadDirectoryChangesW` 버퍼 오버플로 | `need_rescan` | 전체 스캔 | 누락 이벤트 복구 |
| 절전 복귀 | `PBT_APMRESUMEAUTOMATIC` | 스케줄·하트비트 재계산, Uploader nudge | |
| 설정 파일 파싱 실패 | 시작 시 | 기본값으로 실행, 창 표시, 오류 배너. 저장 시 `.bak` 후 덮어씀 | |
| 상태 DB 손상 | 열기/무결성 실패 | `.corrupt-{ts}`로 이동 후 신규 | 전량 재업로드 → DEDUP |
| equip 미입력 | validate | Uploader/Heartbeat Gated, 트레이 "오류", 배너 | 감시·스풀은 계속 |
| 두 패턴이 같은 파일에 매칭 | 매칭 시 | config 순서상 첫 패턴 사용 [설계 결정 D-18] | |
| 작업 타임아웃 | `sleep(timeout)` 선행 | Job Object 종료, `timed_out=1`, warn | |
| 작업 중복 회차 | `running` 맵 | `skipped=1`, warn | |
| 종료 중 조각 처리 진행 | Shutdown | 현재 조각 완료 후 종료 (최대 10초) | |

### 10.3 에러 타입

```rust
pub enum ChunkError { Open(io::Error), FileKey(io::Error), Read(io::Error), Spool(io::Error), Db(rusqlite::Error), NoPattern }
pub enum UploadError { Unc(io::Error, PathBuf), Gated }
pub enum StartError { Db(..), Spool(..), Runtime(..) }
```

GUI 표시용으로 `io::Error` 의 Win32 코드를 한국어 문구로 바꾸는 `win::describe(&io::Error) -> String` 을 둔다
(ACCESS_DENIED, PATH_NOT_FOUND, BAD_NETPATH, NETWORK_UNREACHABLE, SHARING_VIOLATION, DISK_FULL, LOGON_FAILURE).

---

## 11. 테스트 전략

### 11.1 단위 테스트 (core, OS 무관)

| 대상 | 케이스 |
|---|---|
| `paths::normalize` | 대소문자, 구분자, `\\?\` 접두사, 끝 구분자 |
| `paths::fixed_prefix` | `**` 위치별, 드라이브 루트, 메타문자 없는 패턴 |
| `paths::sanitize_equip` | 금지 문자, 공백, 길이, 구분자 변경 |
| `paths::{format,parse}_chunk_name` | 왕복, `__`가 포함된 이름, 하위 경로 `~` 치환 |
| `chunk::cut_at_last_newline` | 개행 없음, 끝이 개행, `\r\n`, 바이너리 잡음 |
| `chunk::field_count` | 따옴표 안 구분자, 빈 필드, 탭 구분 |
| `chunk::synth_header` | `c0..cn`, fixed join |
| `task::next_run` | 정각 경계, n분 배수, daily 지남/안 지남, 자정 넘김 |
| `upload::backoff_delay` | 지수 증가와 상한 |
| `config::validate` | 표 3.3의 각 규칙 |
| `config` diff | 어떤 워커가 반응하는지 |

### 11.2 통합 테스트 (Windows, tempdir)

`core`의 런타임을 임시 디렉토리로 띄우고 UNC 대신 로컬 디렉토리를 `unc_base`로 준다.

- append 시뮬레이터: 부분 줄 쓰기 → 잠시 후 나머지 → 조각 하나에 완전한 줄만 담기는지
- `max_wait` 강제 처리: 계속 쓰는 동안 첫 이벤트 + max_wait 안에 조각이 나오는지
- truncate, 삭제 후 재생성, 이름 변경
- `auto` 첫 조각이 원본 그대로이고 두 번째 조각부터 헤더가 붙는지
- `none` 필드 수 변동 경고와 헤더 재생성
- 스풀 가드: 상한 초과 시 오래된 것부터 삭제
- 전송 실패 주입(대상 디렉토리 읽기 전용) → Backoff → Suspended → nudge로 재개
- 크래시 주입: `cfg(test)` 훅으로 스풀 rename 직후 / offset 커밋 직전 / UNC rename 직후에 패닉 → 재시작 후 중복만 발생하고 유실 없음
- 설정 핫리로드: 패턴 추가 시 초기 스캔, 작업 이름 변경 시 루프 재생성
- 스케줄러: `timeout_sec` 초과 시 자식과 손자 프로세스 모두 종료

### 11.3 수동 검증

- 10분 유휴 CPU 0% (9장 절차)
- 실제 쓰기 프로그램이 열어 둔 파일 읽기 (공유 위반 여부)
- 절전 → 복귀 후 정각 작업 1회만 실행되는지
- 두 번째 실행 시 기존 창이 앞으로 오는지
- 로그오프 후 하트비트 mtime이 멈추는지

---

## 12. 사양 미명시 항목에 대한 설계 결정

| ID | 항목 | 결정 | 근거 |
|---|---|---|---|
| D-01 | 시작 시 초기 스캔 | 시작 시와 패턴 변경 시 root를 1회 훑어 매칭 파일을 처리 큐에 넣는다 | 꺼져 있던 동안 자란 파일은 이벤트가 오지 않는다. 사양 2.4 "처음 발견한 파일은 offset 0"도 발견 절차를 전제한다 |
| D-02 | 감시 등록 실패 재시도 | 별도 타이머 없이 하트비트 주기에 편승 | 유휴 CPU 요구를 지키면서 자동 복구 |
| D-03 | 스풀 레이아웃 | `spool/{YYYYMMDD}/{조각}` , 쓰는 중은 `.tmp` | 날짜가 곧 업로드 경로. 열거만으로 큐를 재구성 |
| D-04 | equip_id 적용 시점 | 전송 시점에 결정. 스풀에는 담지 않음 | 사양 2.1 "이후 조각부터 새 디렉토리"를 "이후 전송부터"로 해석. 잘못 입력한 값을 고쳤을 때 스풀 잔여분이 올바른 디렉토리로 가는 편이 운영상 낫다 |
| D-05 | 조각 이름의 원본파일명 세그먼트 | prefix 기준 상대 경로, 구분자 `~` | `**` 패턴에서 동명 파일 충돌 방지. prefix 바로 아래 파일은 사양과 동일. 13장 참고 |
| D-06 | 조각 최대 크기 | `agent.max_chunk_bytes` 기본 8 MiB, 초과 시 여러 조각 | 초기 전량 업로드 시 메모리와 SMB 전송 단위 제한 |
| D-07 | auto 모드 offset 0 조각 | 헤더를 붙이지 않고 원본 그대로 | 조각 첫 줄이 헤더라는 규칙과 offset이 원본 바이트 위치라는 규칙을 동시에 만족 |
| D-08 | equip 미입력 시 감시 | 감시·스풀은 계속, 전송·하트비트만 Gated | 설정 완료 직후 그동안의 데이터가 바로 올라간다. 스풀 가드가 상한을 지킨다 |
| D-09 | retry_max 초과 후 | Suspended. 새 조각/수동/하트비트 성공/설정/절전 복귀로만 재개 | 무한 백오프 대신 명시적 상태. 하트비트가 네트워크 복구 탐지 역할 |
| D-10 | bytes_sent_today 보존 | `counters` 테이블에 날짜와 함께 저장 | 재시작해도 하트비트 값이 0으로 돌아가지 않도록 |
| D-11 | 작업 출력 보관 | `task_history`에 stdout/stderr 마지막 16 KiB, 전체는 로그 | 탭 2 하단 출력 표시 요구 |
| D-12 | every_n_min 정렬 | 자정 기준 배수 | hourly의 정각 정렬과 일관 |
| D-13 | 즉시 반영 예외 | `spool_dir`, `log.dir`, `log.max_files`는 재시작 후 | 미전송 조각이 두 디렉토리에 흩어지는 상황 방지 |
| D-14 | TOML 주석 | 저장 시 주석 소실 허용, `config.example.toml` 제공 | GUI 편집 전제. `toml_edit` 도입은 복잡도 대비 이득 작음 |
| D-15 | 작업 타임아웃 시 종료 범위 | Job Object로 자식 트리 전체 | 배치 파일이 띄운 손자 프로세스가 남는 문제 방지 |
| D-16 | 상태 DB 손상 | 이름 바꿔 보관 후 새로 생성 | 전량 재업로드는 DEDUP이 흡수. 수동 개입 없이 복구 |
| D-17 | 시작 시 창 표시 | 기본 숨김, 설정 오류가 있으면 표시 | 첫 설치 시 즉시 입력 유도 |
| D-18 | 패턴 다중 매칭 | config 순서상 첫 패턴 | 결정적이고 설명하기 쉬움. GUI 미리보기에서 중복 매칭을 경고 |
| D-19 | 조각 날짜 | 조각 생성 시각의 로컬 날짜 | 전송이 지연돼도 데이터 발생 시점에 가까운 날짜에 배치 |
| D-20 | 삭제된 파일의 DB 행 | 삭제 이벤트에서만 제거, 초기 스캔 미발견은 `present=false` 표시만 | 드라이브 일시 분리 시 offset 보존 |

---

## 13. 사양서 반영 제안

아래는 이 설계가 사양 v0.3과 다르거나 사양에 추가가 필요한 항목이다. 사양 v0.4에 반영을 권한다.

1. **조각 파일명 [spec 2.6]**: `{원본파일명}` 을 "패턴 고정 prefix 기준 상대 경로, 구분자 `~`"로 정의 (D-05).
   서버 파싱 규칙은 그대로다.
2. **auto 모드 offset 0 조각 [spec 2.6]**: 헤더를 덧붙이지 않는다는 규칙 명시 (D-07).
3. **`[[watch]] enabled`** 필드 추가 (탭 3 "활성화" 토글의 저장 위치).
4. **`[[task]]` 스케줄 필드**: `interval_min`, `daily_at` 추가 (탭 2 편집 폼 대응).
5. **`[agent] max_chunk_bytes`** 추가 (D-06).
6. **`retry_max` 의미**: 초과 시 Suspended 상태와 재개 조건 명시 (D-09).
7. **하트비트 필드 추가**: `agent_started_at`, `equip_id_changed_from`, `config_errors`, `watch_errors`, `uploader_state`.
8. **상태 DB**: `counters`, `meta` 테이블과 `task_history` 출력 컬럼 추가 (D-10, D-11).
9. **재시작 필요 항목**: `spool_dir`, `log.dir`, `log.max_files` (D-13).
10. **절전 복귀 알림** 사용 명시 (8.5).

---

## 14. 구현 순서 매핑 [spec 9]

| 단계 | 사양 | 모듈 | 검증 수단 |
|---|---|---|---|
| 1 | config + 로깅 + 이중 실행 방지 | `config`, `logging`, `paths`, `win::{mutex,event}` | 단위 테스트, `cli check-config` |
| 2 | 파일 감시 + 디바운스 (로그만) | `watch`, `debounce`, `bus`, `runtime`(부분) | `cli run --dry-run` 으로 이벤트 로그 확인, 10분 유휴 CPU |
| 3 | 증분 읽기 + 개행 절단 + SQLite | `state`, `chunk`(헤더 제외) | 통합 테스트 append 시뮬레이터 |
| 4 | 헤더 처리 | `chunk::header` | 단위 + 통합 |
| 5 | 조각 생성 + 스풀 | `spool`, `chunk` 완성 | 스풀 파일 검사, `cli chunk-name --parse` |
| 6 | UNC 전송 | `upload`, `win::describe` | 로컬 디렉토리 → 실제 UNC. **여기까지 `cli run`으로 실사용 검증** |
| 7 | 트레이 + 종료 확인 | `app::{tray,single_instance}`, `ui::modal` | 수동 |
| 8 | GUI 탭 1 | `ui::{tab_global,draft}`, `win::registry`, TestUnc | 수동 |
| 9 | GUI 탭 3 | `ui::tab_watch`, PreviewPattern, ResetOffset | 수동 |
| 10 | 주기 명령 + 탭 2 | `task`, `win::job`, `ui::tab_tasks` | 통합(타임아웃) + 수동 |
| 11 | 하트비트 | `heartbeat`, `win::power` | 서버 `_status` 확인, 절전 복귀 |

`cli` 서브커맨드: `run [--dry-run]`, `check-config`, `scan --pattern`, `chunk-name --parse`, `test-unc`.

---

## 부록 A. 크레이트와 feature

| 크레이트 | feature / 비고 |
|---|---|
| `tokio` | `rt-multi-thread, sync, time, process, fs, macros, io-util` |
| `tokio-util` | `CancellationToken` |
| `notify` | 기본 (Windows = ReadDirectoryChangesW). `PollWatcher` 미사용 |
| `walkdir` | 초기 스캔 |
| `glob` | `Pattern`, `MatchOptions` |
| `rusqlite` | `bundled` |
| `flate2` | 기본 (miniz_oxide) |
| `serde`, `toml`, `serde_json` | 설정, 하트비트 |
| `chrono` | `Local`, RFC 3339 |
| `tracing`, `tracing-subscriber` (`reload`), `tracing-appender` | 일 단위 로테이션 `max_log_files(14)`, `non_blocking` |
| `windows` | `Win32_Foundation, Win32_Storage_FileSystem, Win32_System_Threading, Win32_System_Registry, Win32_System_Power, Win32_System_JobObjects` |
| `eframe`, `egui`, `egui_extras` | GUI, 테이블 |
| `tray-icon` | 트레이. 이벤트 핸들러 콜백 방식 |
| `rfd` | 파일 선택 대화상자 |
| `image` | 아이콘 PNG 디코드 (또는 build.rs에서 RGBA 사전 변환) |
| `hostname` | 하트비트 |

## 부록 B. 상태 전이 요약

**progress 행 (파일 하나)**

```
(없음) ──초기 스캔/이벤트──▶ offset=0, header=None
   │                              │ 헤더 파생 성공 + 조각 생성
   │                              ▼
   │                        offset=n, header=Some ──이벤트──▶ offset=m (m>n)
   │                              │ size<offset            │ Removed
   │                              ▼                        ▼
   │                        offset=0, header=None       (삭제)
   └──Removed──▶ (삭제)
```

**Uploader**: 6.5 참고. **Debouncer 항목**: 없음 → Pending(deadline) → 발화 → 없음. 이벤트마다 deadline 재설정.
