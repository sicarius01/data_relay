use crate::{
    bus::TaskStatus,
    config::{Config, ScheduleKind, TaskConfig},
    shared::SharedState,
    state::{StateDb, TaskRun},
};
use anyhow::Context;
use chrono::{DateTime, Local, NaiveTime, TimeZone, Timelike};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    process::Stdio,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    sync::{mpsc, oneshot, watch, Notify},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

pub type TestTaskReply = oneshot::Sender<Result<TaskRun, String>>;
pub enum TaskRequest {
    RunSaved(String),
    Test {
        config: TaskConfig,
        reply: TestTaskReply,
    },
}
struct TaskCompletion {
    row: TaskRun,
    reply: Option<TestTaskReply>,
}

#[derive(Clone)]
struct ConsoleOutput {
    tx: mpsc::Sender<Vec<u8>>,
    footer: Arc<Mutex<Option<String>>>,
    dropped_bytes: Arc<AtomicU64>,
}
impl ConsoleOutput {
    fn display(&self, text: impl Into<Vec<u8>>) {
        // A user can close or pause the console. GUI capture and task timeout must
        // remain independent of its pipe; only the live display may be truncated.
        if let Err(error) = self.tx.try_send(text.into()) {
            self.dropped_bytes
                .fetch_add(error.into_inner().len() as u64, Ordering::Relaxed);
        }
    }
    fn finish(&self, row: &TaskRun) {
        let dropped = self.dropped_bytes.load(Ordering::Relaxed);
        let omitted = if dropped == 0 {
            String::new()
        } else {
            format!("\n[출력 창 전송 생략: {dropped}바이트] GUI 실행 결과에는 마지막 stdout/stderr가 보관됩니다.\n")
        };
        *self.footer.lock().unwrap() = Some(format!(
            "{omitted}\n[테스트 완료] 종료 코드: {} / 제한 시간 초과: {} / 실행 시간: {} ms\n",
            row.exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "없음".into()),
            if row.timed_out { "예" } else { "아니요" },
            row.duration_ms.unwrap_or_default()
        ));
    }
}

#[cfg(all(windows, not(test)))]
async fn open_test_console(
    config: &TaskConfig,
    cancel: CancellationToken,
) -> Result<Option<ConsoleOutput>, String> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let setup=async {
        let mut command=tokio::process::Command::new(std::env::current_exe()?);
        command.arg("--task-console-viewer").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null())
            .creation_flags(0x08000000|0x00000004).kill_on_drop(true);
        let mut child=command.spawn().context("테스트 CMD 출력 창 실행 실패")?;
        let job=match crate::win::ProcessJob::attach(&child) {Ok(job)=>job,Err(error)=>{let _=child.kill().await;return Err(error);}};
        let mut readiness=BufReader::new(child.stdout.take().unwrap());let mut ready=String::new();
        let handshake=tokio::select! {
            _=cancel.cancelled()=>Err(anyhow::anyhow!("에이전트가 종료되어 테스트가 취소되었습니다.")),
            result=tokio::time::timeout(Duration::from_secs(5),readiness.read_line(&mut ready))=>{
                match result {Ok(Ok(_)) if ready.trim()=="READY"=>Ok(()),Ok(Ok(_))=>Err(anyhow::anyhow!("테스트 CMD 출력 창을 초기화하지 못했습니다.")),Ok(Err(error))=>Err(error.into()),Err(_)=>Err(anyhow::anyhow!("테스트 CMD 출력 창 응답 시간이 초과되었습니다."))}
            }
        };
        if let Err(error)=handshake {let _=job.terminate();let _=child.kill().await;return Err(error);}
        let mut input=child.stdin.take().unwrap();
        let (tx,mut rx)=mpsc::channel::<Vec<u8>>(64);
        let footer=Arc::new(Mutex::new(None::<String>));let footer_writer=footer.clone();
        tokio::spawn(async move {
            let display=async {
                while let Some(bytes)=rx.recv().await {if input.write_all(&bytes).await.is_err(){break;}}
                let final_text=footer_writer.lock().unwrap().clone();
                if let Some(final_text)=final_text {let _=input.write_all(final_text.as_bytes()).await;}
                let _=input.shutdown().await;drop(input);
                let _=child.wait().await;
            };
            tokio::select! {_=cancel.cancelled()=>{let _=job.terminate();},_=display=>{}}
            drop(job);
        });
        let output=ConsoleOutput{tx,footer,dropped_bytes:Arc::new(AtomicU64::new(0))};
        output.display(format!("CSV Agent — 테스트 실행\n프로그램: {}\n인수: {:?}\n작업 폴더: {}\n\n",
            config.program,config.args,config.working_dir.as_deref().unwrap_or("(현재 폴더)")));
        Ok::<_,anyhow::Error>(Some(output))
    }.await;
    setup.map_err(|error| crate::win::describe_error(&error))
}

#[cfg(test)]
async fn open_test_console(
    _config: &TaskConfig,
    _cancel: CancellationToken,
) -> Result<Option<ConsoleOutput>, String> {
    Ok(None)
}
#[cfg(all(not(windows), not(test)))]
async fn open_test_console(
    _config: &TaskConfig,
    _cancel: CancellationToken,
) -> Result<Option<ConsoleOutput>, String> {
    Err("CMD 테스트 출력 창은 Windows에서 지원합니다.".into())
}

/// A one-off test needs execution fields only. Scheduling and other agent settings
/// deliberately do not participate; a disabled, unsaved draft is executable.
pub fn validate_test_task(config: &TaskConfig) -> Result<(), String> {
    if config.name.trim().is_empty() {
        return Err("작업 이름을 입력해 주세요.".into());
    }
    if config.program.trim().is_empty() {
        return Err("실행할 프로그램을 입력해 주세요.".into());
    }
    if config.timeout_sec == 0 {
        return Err("제한 시간은 1초 이상이어야 합니다.".into());
    }
    if config.name.contains('\0')
        || config.program.contains('\0')
        || config.args.iter().any(|arg| arg.contains('\0'))
        || config
            .working_dir
            .as_ref()
            .is_some_and(|dir| dir.contains('\0'))
    {
        return Err("작업 이름·프로그램·인수·작업 폴더에 NUL 문자를 사용할 수 없습니다.".into());
    }
    if config
        .working_dir
        .as_ref()
        .is_some_and(|dir| !dir.is_empty() && dir.trim().is_empty())
    {
        return Err("작업 폴더를 입력하거나 비워 주세요.".into());
    }
    Ok(())
}

pub fn next_run(cfg: &TaskConfig, now: DateTime<Local>) -> DateTime<Local> {
    let minute = now.hour() * 60 + now.minute();
    let interval = match cfg.schedule {
        ScheduleKind::Hourly => 60,
        ScheduleKind::EveryNMin => cfg.interval_min.unwrap_or(1).clamp(1, 1440),
        ScheduleKind::Daily => 1440,
    };
    let first = if cfg.schedule == ScheduleKind::Daily {
        cfg.daily_at
            .as_deref()
            .and_then(|s| NaiveTime::parse_from_str(s, "%H:%M").ok())
            .map(|t| t.hour() * 60 + t.minute())
            .unwrap_or(0)
    } else {
        (minute / interval) * interval
    };
    // Resolve each wall-clock candidate independently. Nonexistent DST times are
    // skipped and either occurrence of an ambiguous time can be the next run.
    for day in 0..4 {
        let date = now.date_naive() + chrono::Duration::days(day);
        let start = if day == 0 || cfg.schedule == ScheduleKind::Daily {
            first
        } else {
            0
        };
        let mut m = start;
        while m < 1440 {
            let naive = date.and_hms_opt(m / 60, m % 60, 0).unwrap();
            let local = Local.from_local_datetime(&naive);
            let mut possible = vec![];
            if let Some(t) = local.earliest() {
                possible.push(t);
            }
            if let Some(t) = local.latest() {
                possible.push(t);
            }
            possible.sort();
            if let Some(t) = possible.into_iter().find(|t| *t > now) {
                return t;
            }
            if cfg.schedule == ScheduleKind::Daily {
                break;
            }
            m += interval;
        }
    }
    now + chrono::Duration::days(1)
}

fn new_run(name: &str, skipped: bool) -> TaskRun {
    TaskRun {
        id: 0,
        task_name: name.into(),
        started_at: Local::now().to_rfc3339(),
        finished_at: None,
        exit_code: None,
        timed_out: false,
        skipped,
        duration_ms: None,
        stdout_tail: None,
        stderr_tail: None,
    }
}

#[derive(Default)]
struct StreamUtf8 {
    pending: Vec<u8>,
}
impl StreamUtf8 {
    fn decode(&mut self, bytes: &[u8], eof: bool) -> String {
        self.pending.extend_from_slice(bytes);
        let mut text = String::new();
        let mut consumed = 0;
        while consumed < self.pending.len() {
            match std::str::from_utf8(&self.pending[consumed..]) {
                Ok(valid) => {
                    text.push_str(valid);
                    consumed = self.pending.len();
                }
                Err(error) => {
                    let end = consumed + error.valid_up_to();
                    text.push_str(std::str::from_utf8(&self.pending[consumed..end]).unwrap());
                    consumed = end;
                    if let Some(invalid) = error.error_len() {
                        text.push('\u{fffd}');
                        consumed += invalid;
                    } else {
                        if eof {
                            text.push('\u{fffd}');
                            consumed = self.pending.len();
                        }
                        break;
                    }
                }
            }
        }
        self.pending.drain(..consumed);
        // Incomplete UTF-8 prefixes are at most three bytes and belong to this
        // stream only; stderr can never supply stdout's missing continuation.
        text
    }
}

fn display_stream(console: &ConsoleOutput, text: &str, stream: &str, line_start: &mut bool) {
    if text.is_empty() {
        return;
    }
    if stream == "stderr" {
        let mut tagged = String::new();
        for piece in text.split_inclusive('\n') {
            if *line_start {
                tagged.push_str("[stderr] ");
            }
            tagged.push_str(piece);
            *line_start = piece.ends_with('\n');
        }
        console.display(tagged);
    } else {
        console.display(text.to_owned());
    }
}

async fn output_tail(
    mut pipe: impl AsyncRead + Unpin,
    name: String,
    stream: &'static str,
    console: Option<ConsoleOutput>,
) -> String {
    let mut tail = VecDeque::with_capacity(16 * 1024);
    let mut buf = [0u8; 4096];
    let mut logged = 0usize;
    let mut clipped = false;
    let mut decoder = StreamUtf8::default();
    let mut line_start = true;
    loop {
        match pipe.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                if let Some(console) = &console {
                    let text = decoder.decode(&buf[..n], false);
                    display_stream(console, &text, stream, &mut line_start);
                }
                if logged < 1024 * 1024 {
                    let take = n.min(1024 * 1024 - logged);
                    tracing::info!(task=%name,stream,output=%String::from_utf8_lossy(&buf[..take]),"task output");
                    logged += take;
                } else if !clipped {
                    tracing::warn!(task=%name,stream,"task log output exceeds 1 MiB; further output omitted");
                    clipped = true;
                }
                for &byte in &buf[..n] {
                    if tail.len() == 16 * 1024 {
                        tail.pop_front();
                    }
                    tail.push_back(byte);
                }
            }
            Err(e) => {
                tracing::warn!(task=%name,stream,error=%e,"output read failed");
                break;
            }
        }
    }
    if let Some(console) = &console {
        display_stream(console, &decoder.decode(&[], true), stream, &mut line_start);
    }
    String::from_utf8_lossy(&tail.into_iter().collect::<Vec<_>>()).into_owned()
}

async fn execute(
    cfg: &TaskConfig,
    cancel: CancellationToken,
    console: Option<&ConsoleOutput>,
) -> anyhow::Result<(Option<i32>, bool, String, String)> {
    let mut command = tokio::process::Command::new(&cfg.program);
    command
        .args(&cfg.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(dir) = cfg.working_dir.as_ref().filter(|s| !s.is_empty()) {
        command.current_dir(dir);
    }
    #[cfg(windows)]
    command.creation_flags(0x08000000 | 0x00000004); // CREATE_NO_WINDOW | CREATE_SUSPENDED
    let mut child = command
        .spawn()
        .with_context(|| format!("작업 프로그램 실행 실패: {}", cfg.program))?;
    #[cfg(windows)]
    let job = match crate::win::ProcessJob::attach(&child) {
        Ok(job) => job,
        Err(error) => {
            let _ = child.kill().await;
            return Err(error);
        }
    };
    let stdout = tokio::spawn(output_tail(
        child.stdout.take().unwrap(),
        cfg.name.clone(),
        "stdout",
        console.cloned(),
    ));
    let stderr = tokio::spawn(output_tail(
        child.stderr.take().unwrap(),
        cfg.name.clone(),
        "stderr",
        console.cloned(),
    ));
    let mut timed_out = false;
    let status = tokio::select! {
        result=child.wait()=>result?,
        _=tokio::time::sleep(Duration::from_secs(cfg.timeout_sec.max(1)))=>{
            timed_out=true;
            #[cfg(windows)]
            let _=job.terminate();
            let _=child.kill().await;child.wait().await?
        }
        _=cancel.cancelled()=>{
            #[cfg(windows)]
            let _=job.terminate();
            let _=child.kill().await;child.wait().await?
        }
    };
    // Once the command exits, close its job before draining pipes; otherwise a
    // grandchild inheriting stdout could keep output readers waiting indefinitely.
    #[cfg(windows)]
    drop(job);
    let (out, err) = tokio::join!(stdout, stderr);
    Ok((status.code(), timed_out, out?, err?))
}

async fn run_once(
    cfg: TaskConfig,
    db: Arc<StateDb>,
    shared: Arc<SharedState>,
    cancel: CancellationToken,
    console: Option<ConsoleOutput>,
) -> TaskRun {
    let mut row = new_run(&cfg.name, false);
    let begin = std::time::Instant::now();
    let insert = row.clone();
    let database = db.clone();
    match tokio::task::spawn_blocking(move || database.insert_task_run(&insert)).await {
        Ok(Ok(id)) => row.id = id,
        other => {
            shared.error("task", format!("could not record task start: {other:?}"));
        }
    }
    match execute(&cfg, cancel, console.as_ref()).await {
        Ok((code, timed_out, out, err)) => {
            row.exit_code = code;
            row.timed_out = timed_out;
            row.stdout_tail = Some(out);
            row.stderr_tail = Some(err);
        }
        Err(e) => {
            let message = crate::win::describe_error(&e);
            row.stderr_tail = Some(message.clone());
            shared.error("task", format!("{}: {message}", cfg.name));
        }
    }
    row.duration_ms = Some(begin.elapsed().as_millis().min(u64::MAX as u128) as u64);
    row.finished_at = Some(Local::now().to_rfc3339());
    if let Some(console) = &console {
        if row.exit_code.is_none() {
            console.display(format!(
                "\n[실행 실패] {}\n",
                row.stderr_tail.as_deref().unwrap_or("알 수 없는 오류")
            ));
        }
        console.finish(&row);
    }
    let completed = row.clone();
    if let Err(e) = tokio::task::spawn_blocking(move || db.finish_task_run(&completed))
        .await
        .map_err(anyhow::Error::from)
        .and_then(|r| r)
    {
        shared.error("task", e.to_string());
    }
    if row.timed_out || row.exit_code != Some(0) {
        tracing::warn!(task=%row.task_name,code=?row.exit_code,timed_out=row.timed_out,"task finished");
    } else {
        tracing::info!(task=%row.task_name,duration_ms=?row.duration_ms,"task finished");
    }
    row
}

async fn trigger(
    name: &str,
    config: &Config,
    running: &mut HashSet<String>,
    jobs: &mut JoinSet<TaskCompletion>,
    db: Arc<StateDb>,
    shared: Arc<SharedState>,
    cancel: CancellationToken,
) {
    let Some(cfg) = config.task.iter().find(|t| t.name == name).cloned() else {
        return;
    };
    if !running.insert(name.into()) {
        let mut row = new_run(name, true);
        row.finished_at = Some(row.started_at.clone());
        row.duration_ms = Some(0);
        let database = db.clone();
        let insert = row.clone();
        if let Ok(Ok(id)) =
            tokio::task::spawn_blocking(move || database.insert_task_run(&insert)).await
        {
            row.id = id;
        }
        tracing::warn!(
            task = name,
            "task occurrence skipped because previous run is active"
        );
        shared.update(|s| {
            if let Some(t) = s.tasks.iter_mut().find(|t| t.name == name) {
                t.last = Some(row);
            }
        });
        return;
    }
    start_job(cfg, None, None, jobs, db, shared, cancel);
}

fn start_job(
    config: TaskConfig,
    reply: Option<TestTaskReply>,
    console: Option<ConsoleOutput>,
    jobs: &mut JoinSet<TaskCompletion>,
    db: Arc<StateDb>,
    shared: Arc<SharedState>,
    cancel: CancellationToken,
) {
    shared.update(|s| {
        if let Some(task) = s.tasks.iter_mut().find(|task| task.name == config.name) {
            task.running = true;
        } else {
            s.tasks.push(TaskStatus {
                name: config.name.clone(),
                enabled: false,
                next_run: None,
                running: true,
                last: None,
            });
        }
    });
    jobs.spawn(async move {
        TaskCompletion {
            row: run_once(config, db, shared, cancel, console).await,
            reply,
        }
    });
}

fn finish_job(completion: TaskCompletion, running: &mut HashSet<String>, shared: &SharedState) {
    let TaskCompletion { row, reply } = completion;
    running.remove(&row.task_name);
    shared.update(|s| {
        if let Some(task) = s.tasks.iter_mut().find(|task| task.name == row.task_name) {
            task.running = false;
            task.last = Some(row.clone());
        }
    });
    if let Some(reply) = reply {
        let _ = reply.send(Ok(row));
        shared.repaint();
    }
}

fn publish_tasks(
    config: &Config,
    due: &HashMap<String, DateTime<Local>>,
    running: &HashSet<String>,
    shared: &SharedState,
) {
    shared.update(|s| {
        let draft_running: Vec<_> = s
            .tasks
            .iter()
            .filter(|task| {
                running.contains(&task.name) && !config.task.iter().any(|cfg| cfg.name == task.name)
            })
            .cloned()
            .collect();
        s.tasks = config
            .task
            .iter()
            .map(|cfg| TaskStatus {
                name: cfg.name.clone(),
                enabled: cfg.enabled,
                next_run: due.get(&cfg.name).copied(),
                running: running.contains(&cfg.name),
                last: s
                    .tasks
                    .iter()
                    .find(|t| t.name == cfg.name)
                    .and_then(|t| t.last.clone()),
            })
            .collect();
        s.tasks.extend(draft_running);
    });
}

pub async fn run(
    mut config: watch::Receiver<Arc<Config>>,
    mut manual: mpsc::UnboundedReceiver<TaskRequest>,
    db: Arc<StateDb>,
    shared: Arc<SharedState>,
    resume: Arc<Notify>,
    cancel: CancellationToken,
) {
    let mut active = config.borrow().clone();
    let mut running = HashSet::new();
    let mut jobs = JoinSet::new();
    let mut due: HashMap<_, _> = active
        .task
        .iter()
        .filter(|t| t.enabled)
        .map(|t| (t.name.clone(), next_run(t, Local::now())))
        .collect();
    publish_tasks(&active, &due, &running, &shared);
    for t in active.task.iter().filter(|t| t.enabled && t.run_on_start) {
        trigger(
            &t.name,
            &active,
            &mut running,
            &mut jobs,
            db.clone(),
            shared.clone(),
            cancel.clone(),
        )
        .await;
    }
    loop {
        let next = due.values().min().copied();
        let wait = next.map(|t| {
            (t - Local::now())
                .to_std()
                .unwrap_or_default()
                .min(Duration::from_secs(3600))
        });
        tokio::select! {
            _=cancel.cancelled()=>break,
            result=jobs.join_next(),if !jobs.is_empty()=>{
                match result {
                    Some(Ok(completion))=>finish_job(completion,&mut running,&shared),
                    Some(Err(e))=>shared.error("task",e.to_string()),None=>{}
                }
            }
            r=config.changed()=>{
                if r.is_err(){break;}
                let new=config.borrow().clone();
                if new.task!=active.task {
                    due.retain(|name,_|new.task.iter().any(|t|t.enabled && &t.name==name));
                    for t in new.task.iter().filter(|t|t.enabled) {
                        if !active.task.iter().any(|old|old==t){due.insert(t.name.clone(),next_run(t,Local::now()));}
                    }
                    publish_tasks(&new,&due,&running,&shared);
                }
                active=new;
            }
            Some(request)=manual.recv()=>match request {
                TaskRequest::RunSaved(name)=>trigger(&name,&active,&mut running,&mut jobs,db.clone(),shared.clone(),cancel.clone()).await,
                TaskRequest::Test{config,reply}=>{
                    if let Err(error)=validate_test_task(&config) {let _=reply.send(Err(error));shared.repaint();}
                    else if !running.insert(config.name.clone()) {
                        let _=reply.send(Err(format!("'{}' 작업이 이미 실행 중입니다. 완료 후 다시 테스트해 주세요.",config.name)));
                        shared.repaint();
                    } else {
                        match open_test_console(&config,cancel.clone()).await {
                            Ok(console)=>start_job(config,Some(reply),console,&mut jobs,db.clone(),shared.clone(),cancel.clone()),
                            Err(error)=>{running.remove(&config.name);let _=reply.send(Err(error));shared.repaint();}
                        }
                    }
                }
            },
            _=resume.notified()=>{
                for t in active.task.iter().filter(|t|t.enabled){due.insert(t.name.clone(),next_run(t,Local::now()));}
                publish_tasks(&active,&due,&running,&shared);
            }
            _=async {match wait {Some(wait)=>tokio::time::sleep(wait).await,None=>std::future::pending::<()>().await}}=>{
                let now=Local::now();
                let ready:Vec<_>=due.iter().filter(|(_,at)|**at<=now).map(|(name,_)|name.clone()).collect();
                for name in ready {
                    if let Some(t)=active.task.iter().find(|t|t.name==name){due.insert(name.clone(),next_run(t,now));}
                    trigger(&name,&active,&mut running,&mut jobs,db.clone(),shared.clone(),cancel.clone()).await;
                }
                publish_tasks(&active,&due,&running,&shared);
            }
        }
    }
    // Cancellation kills process trees inside run_once and persists their result.
    while let Some(result) = jobs.join_next().await {
        match result {
            Ok(completion) => finish_job(completion, &mut running, &shared),
            Err(e) => shared.error("task", e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    fn powershell_task(name: &str, script: &str) -> TaskConfig {
        TaskConfig {
            name: name.into(),
            program: "powershell.exe".into(),
            args: vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                script.into(),
            ],
            enabled: false,
            run_on_start: true,
            schedule: ScheduleKind::Daily,
            daily_at: Some("invalid-for-scheduling".into()),
            timeout_sec: 10,
            ..Default::default()
        }
    }
    #[cfg(windows)]
    fn test_agent(dir: &std::path::Path, tasks: Vec<TaskConfig>) -> crate::runtime::AgentHandle {
        let paths = crate::paths::AppPaths::from_base(dir.join("agent"));
        let mut config = Config::default();
        config.agent.spool_dir = paths.spool.to_string_lossy().into_owned();
        config.agent.autostart = false;
        config.task = tasks;
        crate::runtime::start(config, paths, Arc::new(|| {})).unwrap()
    }
    #[cfg(windows)]
    async fn request_test(
        agent: &crate::runtime::AgentHandle,
        config: TaskConfig,
    ) -> Result<TaskRun, String> {
        let (reply, rx) = oneshot::channel();
        agent
            .send(crate::bus::Command::TestTask { config, reply })
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), rx)
            .await
            .unwrap()
            .unwrap()
    }
    #[cfg(windows)]
    async fn history(agent: &crate::runtime::AgentHandle, name: &str) -> Vec<TaskRun> {
        let (reply, rx) = oneshot::channel();
        agent
            .send(crate::bus::Command::TaskHistory {
                name: name.into(),
                reply,
            })
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .unwrap()
            .unwrap()
    }
    #[cfg(windows)]
    async fn wait_running(agent: &crate::runtime::AgentHandle, name: &str) {
        let mut status = agent.status_rx.clone();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if status
                    .borrow_and_update()
                    .tasks
                    .iter()
                    .any(|task| task.name == name && task.running)
                {
                    break;
                }
                status.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
    }
    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn disabled_unsaved_test_executes_once_and_keeps_config_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let agent = test_agent(dir.path(), vec![]);
        let applied = agent.config_rx.borrow().clone();
        let draft=powershell_task("unsaved-draft","[Console]::Out.WriteLine('draft-output'); [Console]::Error.WriteLine('draft-error'); exit 7");
        let row = request_test(&agent, draft).await.unwrap();
        assert_eq!(row.exit_code, Some(7));
        assert!(!row.timed_out);
        assert!(!row.skipped);
        assert!(row.stdout_tail.as_deref().unwrap().contains("draft-output"));
        assert!(row.stderr_tail.as_deref().unwrap().contains("draft-error"));
        assert_eq!(*agent.config_rx.borrow().clone(), *applied);
        assert_eq!(history(&agent, "unsaved-draft").await.len(), 1);
        assert!(!dir.path().join("agent/config.toml").exists());
        assert!(agent
            .snapshot()
            .tasks
            .iter()
            .all(|task| task.next_run.is_none()));
        agent.shutdown().unwrap();
    }
    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn draft_timeout_and_saved_manual_overlap_share_the_same_guard() {
        let dir = tempfile::tempdir().unwrap();
        let saved = powershell_task("overlap", "[Console]::Out.WriteLine('must-not-run')");
        let agent = test_agent(dir.path(), vec![saved]);
        let mut draft = powershell_task("overlap", "Start-Sleep -Seconds 30");
        draft.timeout_sec = 1;
        let (reply, rx) = oneshot::channel();
        agent
            .send(crate::bus::Command::TestTask {
                config: draft.clone(),
                reply,
            })
            .unwrap();
        wait_running(&agent, "overlap").await;
        agent
            .send(crate::bus::Command::RunTaskNow("overlap".into()))
            .unwrap();
        let duplicate = request_test(&agent, draft).await.unwrap_err();
        assert!(duplicate.contains("이미 실행 중"));
        let row = tokio::time::timeout(Duration::from_secs(8), rx)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(row.timed_out);
        let rows = history(&agent, "overlap").await;
        assert_eq!(rows.iter().filter(|row| !row.skipped).count(), 1);
        assert_eq!(rows.iter().filter(|row| row.skipped).count(), 1);
        assert!(!rows.iter().any(|row| row
            .stdout_tail
            .as_deref()
            .is_some_and(|out| out.contains("must-not-run"))));
        assert!(!agent.config_rx.borrow().task[0].enabled);
        agent.shutdown().unwrap();
    }
    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scheduled_startup_run_rejects_overlapping_draft_test() {
        let dir = tempfile::tempdir().unwrap();
        let mut saved = powershell_task("scheduled", "Start-Sleep -Seconds 30");
        saved.enabled = true;
        saved.schedule = ScheduleKind::Hourly;
        saved.timeout_sec = 30;
        let agent = test_agent(dir.path(), vec![saved]);
        wait_running(&agent, "scheduled").await;
        let error = request_test(&agent, powershell_task("scheduled", "exit 0"))
            .await
            .unwrap_err();
        assert!(error.contains("이미 실행 중"));
        agent.shutdown().unwrap();
    }
    #[test]
    fn test_validation_requires_execution_fields_only() {
        let mut cfg = TaskConfig {
            name: "test".into(),
            program: "program-from-path".into(),
            enabled: false,
            schedule: ScheduleKind::Daily,
            daily_at: None,
            ..Default::default()
        };
        assert!(validate_test_task(&cfg).is_ok());
        cfg.timeout_sec = 0;
        assert!(validate_test_task(&cfg).is_err());
        cfg.timeout_sec = 1;
        cfg.args.push("bad\0argument".into());
        assert!(validate_test_task(&cfg).is_err());
    }
    fn at(h: u32, m: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 9, 10, h, m, 0).unwrap()
    }
    #[test]
    fn schedules_are_wall_clock_aligned() {
        let mut cfg = TaskConfig::default();
        assert_eq!(next_run(&cfg, at(12, 0)), at(13, 0));
        cfg.schedule = ScheduleKind::EveryNMin;
        cfg.interval_min = Some(17);
        assert_eq!(next_run(&cfg, at(12, 0)), at(12, 11));
        assert_eq!(
            next_run(&cfg, at(23, 59)),
            at(0, 0) + chrono::Duration::days(1)
        );
        cfg.schedule = ScheduleKind::Daily;
        cfg.daily_at = Some("15:30".into());
        assert_eq!(next_run(&cfg, at(12, 0)), at(15, 30));
        assert_eq!(
            next_run(&cfg, at(15, 30)),
            at(15, 30) + chrono::Duration::days(1)
        );
    }
    #[tokio::test]
    async fn output_is_drained_with_a_bounded_tail() {
        let data = vec![b'x'; 40 * 1024];
        let out = output_tail(&data[..], "test".into(), "stdout", None).await;
        assert_eq!(out.len(), 16 * 1024);
    }
    #[tokio::test]
    async fn paused_console_cannot_block_output_capture() {
        let (tx, _paused_receiver) = mpsc::channel(1);
        let console = ConsoleOutput {
            tx,
            footer: Arc::new(Mutex::new(None)),
            dropped_bytes: Arc::new(AtomicU64::new(0)),
        };
        let data = vec![b'x'; 40 * 1024];
        let captured = tokio::time::timeout(
            Duration::from_secs(5),
            output_tail(&data[..], "test".into(), "stdout", Some(console.clone())),
        )
        .await
        .unwrap();
        assert_eq!(captured.len(), 16 * 1024);
        assert!(console.dropped_bytes.load(Ordering::Relaxed) > 0);
        console.finish(&new_run("test", false));
        assert!(console
            .footer
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .contains("전송 생략"));
    }
    #[tokio::test]
    async fn console_preserves_korean_split_between_every_read() {
        struct ByteReader<'a>(&'a [u8]);
        impl AsyncRead for ByteReader<'_> {
            fn poll_read(
                mut self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
                buf: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                if !self.0.is_empty() {
                    buf.put_slice(&self.0[..1]);
                    self.0 = &self.0[1..];
                }
                std::task::Poll::Ready(Ok(()))
            }
        }
        let (tx, mut rx) = mpsc::channel(64);
        let console = ConsoleOutput {
            tx,
            footer: Arc::new(Mutex::new(None)),
            dropped_bytes: Arc::new(AtomicU64::new(0)),
        };
        let message = "한글 출력🙂\n두 번째 줄\n";
        let captured = output_tail(
            ByteReader(message.as_bytes()),
            "split".into(),
            "stderr",
            Some(console.clone()),
        )
        .await;
        assert_eq!(captured, message);
        drop(console);
        let mut displayed = Vec::new();
        while let Some(bytes) = rx.recv().await {
            displayed.extend(bytes);
        }
        assert_eq!(
            String::from_utf8(displayed).unwrap(),
            "[stderr] 한글 출력🙂\n[stderr] 두 번째 줄\n"
        );
    }
}
