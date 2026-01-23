use chrono::{DateTime, Datelike, Local, Timelike};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use log::{LevelFilter, Metadata, Record};
use std::{
    cell::RefCell,
    io::{BufWriter, Write},
    sync::Arc,
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use arrayvec::ArrayString;

pub use log::{debug, error, info, trace, warn};
pub type Level = LevelFilter;
const CHANNEL_CAPACITY: usize = 65_536;
const INLINE_MSG_CAP: usize = 256;
const ENTRY_BATCH_SIZE: usize = 32;

thread_local! {
    static TS_CACHE: RefCell<ThreadTimestampCache> =
        RefCell::new(ThreadTimestampCache::new());
    static ENTRY_BUFFER: RefCell<Vec<LogEntry>> =
        RefCell::new(Vec::with_capacity(ENTRY_BATCH_SIZE));
}

#[derive(Debug, Clone, Copy)]
struct Timestamp {
    secs: u64,
    nanos: u32,
}

#[derive(Debug)]
struct LogEntry {
    ts: Timestamp,
    name: Option<Arc<str>>,
    level: log::Level,
    msg: LogMessage,
}

impl LogEntry {
    #[inline]
    pub fn ts(&self) -> Timestamp {
        self.ts
    }
    #[inline]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
    #[inline]
    pub fn level(&self) -> log::Level {
        self.level
    }
    #[inline]
    pub fn msg(&self) -> &str {
        self.msg.as_str()
    }
}

#[derive(Debug)]
enum LogMessage {
    Inline(ArrayString<INLINE_MSG_CAP>),
    Heap(String),
}

impl LogMessage {
    #[inline]
    fn as_str(&self) -> &str {
        match self {
            LogMessage::Inline(msg) => msg.as_str(),
            LogMessage::Heap(msg) => msg.as_str(),
        }
    }
}

struct ThreadTimestampCache {
    base_instant: Instant,
    base_secs: u64,
    base_nanos: u32,
}

impl ThreadTimestampCache {
    fn new() -> Self {
        let ts = now_timestamp();
        Self {
            base_instant: Instant::now(),
            base_secs: ts.secs,
            base_nanos: ts.nanos,
        }
    }

    fn refresh(&mut self) -> Timestamp {
        let ts = now_timestamp();
        self.base_instant = Instant::now();
        self.base_secs = ts.secs;
        self.base_nanos = ts.nanos;
        ts
    }

    fn now(&mut self) -> Timestamp {
        let elapsed = self.base_instant.elapsed();
        if elapsed >= Duration::from_secs(1) {
            return self.refresh();
        }

        let elapsed_nanos = elapsed.as_nanos() as u64;
        let total_nanos = self.base_nanos as u64 + elapsed_nanos;
        Timestamp {
            secs: self.base_secs + (total_nanos / 1_000_000_000),
            nanos: (total_nanos % 1_000_000_000) as u32,
        }
    }
}
enum Action {
    WriteBatch(Vec<LogEntry>),
    Flush,
    Exit,
}

#[derive(Debug)]
struct Context<P: ToString + Send> {
    rx: Receiver<Action>,
    path: Option<P>,
    date: chrono::NaiveDate,
    unix_ts: bool,
}

pub struct Handle {
    tx: Sender<Action>,
    thread: Option<JoinHandle<()>>,
}

impl Handle {
    pub fn stop(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = self.tx.send(Action::Exit);
            let _ = thread.join();
        }
    }
}
impl Drop for Handle {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Logger {
    tx: Sender<Action>,
    name: Option<Arc<str>>,
}

impl log::Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let msg = {
            let mut inline = ArrayString::<INLINE_MSG_CAP>::new();
            use std::fmt::Write as _;
            if write!(&mut inline, "{}", record.args()).is_ok() {
                LogMessage::Inline(inline)
            } else {
                LogMessage::Heap(record.args().to_string())
            }
        };

        let entry = LogEntry {
            ts: cached_timestamp(),
            name: self.name.as_ref().map(Arc::clone),
            level: record.level(),
            msg,
        };

        push_entry(&self.tx, entry);
    }

    fn flush(&self) {
        flush_thread_buffer(&self.tx);
        let _ = self.tx.send(Action::Flush);
    }
}

fn open_file(path: &str) -> Result<std::fs::File, std::io::Error> {
    let dir = std::path::Path::new(path);
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }

    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

fn rotate<P: ToString + Send>(
    ctx: &Context<P>,
) -> Result<BufWriter<Box<dyn Write>>, std::io::Error> {
    let capacity = 1024 * 1024;
    match &ctx.path {
        Some(path) => {
            let path = {
                let postfix = ctx.date.format("_%Y%m%d").to_string();
                let path_str = path.to_string();
                let input = std::path::Path::new(&path_str);
                let stem = input.file_stem().and_then(|s| s.to_str());
                let ext = input.extension().and_then(|s| s.to_str());
                if let (Some(stem), Some(ext)) = (stem, ext) {
                    let filename = format!("{stem}{postfix}.{ext}");
                    match input.parent() {
                        Some(parent) if !parent.as_os_str().is_empty() => {
                            parent.join(filename).to_string_lossy().to_string()
                        }
                        _ => filename,
                    }
                } else {
                    format!("{}{}.log", path_str, postfix)
                }
            };
            let file = open_file(&path)?;
            Ok(BufWriter::with_capacity(capacity, Box::new(file)))
        }
        None => {
            let target = Box::new(std::io::stdout());
            Ok(BufWriter::with_capacity(capacity, target))
        }
    }
}

fn now_timestamp() -> Timestamp {
    let now = SystemTime::now();
    let since_epoch = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|err| err.duration());
    Timestamp {
        secs: since_epoch.as_secs(),
        nanos: since_epoch.subsec_nanos(),
    }
}

fn cached_timestamp() -> Timestamp {
    TS_CACHE.with(|cache| cache.borrow_mut().now())
}

struct TimestampCache {
    last_secs: u64,
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    offset_sign: char,
    offset_h: i32,
    offset_m: i32,
    date: chrono::NaiveDate,
    time_prefix: String,
    offset_prefix: String,
    unix_prefix: String,
}

impl TimestampCache {
    fn new() -> Self {
        Self {
            last_secs: u64::MAX,
            year: 0,
            month: 0,
            day: 0,
            hour: 0,
            minute: 0,
            second: 0,
            offset_sign: '+',
            offset_h: 0,
            offset_m: 0,
            date: chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap(),
            time_prefix: String::new(),
            offset_prefix: String::new(),
            unix_prefix: String::new(),
        }
    }

    fn update(&mut self, secs: u64) {
        if self.last_secs == secs {
            return;
        }

        let dt: DateTime<Local> = DateTime::from(UNIX_EPOCH + Duration::from_secs(secs));
        self.last_secs = secs;
        self.year = dt.year();
        self.month = dt.month();
        self.day = dt.day();
        self.hour = dt.hour();
        self.minute = dt.minute();
        self.second = dt.second();
        self.date = dt.date_naive();

        let offset = dt.offset().local_minus_utc();
        self.offset_sign = if offset >= 0 { '+' } else { '-' };
        let offset_abs = offset.abs();
        self.offset_h = offset_abs / 3600;
        self.offset_m = (offset_abs % 3600) / 60;

        self.time_prefix = format!(
            "time={:04}-{:02}-{:02}T{:02}:{:02}:{:02}.",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        );
        self.offset_prefix =
            format!("{}{:02}:{:02} level=", self.offset_sign, self.offset_h, self.offset_m);
        self.unix_prefix = format!("time={}.", secs);
    }
}

fn worker<P: ToString + Send>(mut ctx: Context<P>) -> Result<(), std::io::Error> {
    let timeout = Duration::from_secs(1);

    let mut target = rotate(&ctx)?;
    let mut last_flush = Instant::now();
    let mut cache = TimestampCache::new();
    loop {
        match ctx.rx.recv_timeout(timeout) {
            Ok(Action::WriteBatch(entries)) => {
                for entry in entries {
                    write_entry(&mut target, &mut ctx, &mut cache, entry)?;
                }
            }
            Ok(Action::Flush) => {
                target.flush()?;
            }
            Ok(Action::Exit) => {
                target.flush()?;
                break;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        if last_flush.elapsed() >= Duration::from_secs(1) {
            last_flush = Instant::now();
            target.flush()?;
        }
    }

    Ok(())
}

fn write_entry<P: ToString + Send>(
    target: &mut BufWriter<Box<dyn Write>>,
    ctx: &mut Context<P>,
    cache: &mut TimestampCache,
    entry: LogEntry,
) -> Result<(), std::io::Error> {
    let ts = entry.ts();
    cache.update(ts.secs);

    if cache.date != ctx.date {
        ctx.date = cache.date;
        *target = rotate(ctx)?;
    }

    let level = match entry.level() {
        log::Level::Trace => "trace",
        log::Level::Debug => "debug",
        log::Level::Info => "info",
        log::Level::Warn => "warn",
        log::Level::Error => "error",
    };

    if ctx.unix_ts {
        target.write_all(cache.unix_prefix.as_bytes())?;
        write!(target, "{:09} level={}", ts.nanos, level)?;
    } else {
        target.write_all(cache.time_prefix.as_bytes())?;
        write!(target, "{:06}", ts.nanos / 1_000)?;
        target.write_all(cache.offset_prefix.as_bytes())?;
        target.write_all(level.as_bytes())?;
    }

    if let Some(name) = entry.name() {
        target.write_all(b" name=")?;
        target.write_all(name.as_bytes())?;
    }
    target.write_all(b" msg=\"")?;
    target.write_all(entry.msg().as_bytes())?;
    target.write_all(b"\"\n")?;
    Ok(())
}

fn push_entry(tx: &Sender<Action>, entry: LogEntry) {
    ENTRY_BUFFER.with(|buffer| {
        let mut buffer = buffer.borrow_mut();
        buffer.push(entry);
        if buffer.len() >= ENTRY_BATCH_SIZE {
            let mut batch = Vec::with_capacity(ENTRY_BATCH_SIZE);
            std::mem::swap(&mut *buffer, &mut batch);
            let _ = tx.send(Action::WriteBatch(batch));
        }
    });
}

fn flush_thread_buffer(tx: &Sender<Action>) {
    ENTRY_BUFFER.with(|buffer| {
        let mut buffer = buffer.borrow_mut();
        if !buffer.is_empty() {
            let mut batch = Vec::with_capacity(buffer.len());
            std::mem::swap(&mut *buffer, &mut batch);
            let _ = tx.send(Action::WriteBatch(batch));
        }
    });
}

pub fn init<P: ToString + Send + 'static>(name: &str, path: Option<P>, level: Level) -> Handle {
    let (tx, rx) = crossbeam_channel::bounded(CHANNEL_CAPACITY);

    let ctx = Context {
        rx,
        path,
        date: Local::now().date_naive(),
        unix_ts: false,
    };

    let logger = Logger {
        tx: tx.clone(),
        name: Some(Arc::from(name)),
    };

    log::set_boxed_logger(Box::new(logger)).expect("error to init logger");
    log::set_max_level(level);

    let thread = std::thread::spawn(move || {
        if let Err(msg) = worker(ctx) {
            eprintln!("error {}", msg);
        }
    });

    Handle {
        tx,
        thread: Some(thread),
    }
}

// Python bindings - instance-based logger
#[cfg(feature = "python")]
mod python {
    use super::{
        cached_timestamp, flush_thread_buffer, push_entry, worker, Action, Context, LogEntry,
        LogMessage, LevelFilter, CHANNEL_CAPACITY, INLINE_MSG_CAP,
    };
    use chrono::Local;
    use crossbeam_channel::Sender;
    use pyo3::prelude::*;
    use std::collections::HashMap;
    use std::hash::{Hash, Hasher};
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::sync::{Arc, Mutex, OnceLock, Weak};
    use std::thread::JoinHandle;
    use arrayvec::ArrayString;

    #[derive(Clone, Eq)]
    enum PathKey {
        Stdout,
        File(String),
    }

    impl PartialEq for PathKey {
        fn eq(&self, other: &Self) -> bool {
            match (self, other) {
                (PathKey::Stdout, PathKey::Stdout) => true,
                (PathKey::File(a), PathKey::File(b)) => a == b,
                _ => false,
            }
        }
    }

    impl Hash for PathKey {
        fn hash<H: Hasher>(&self, state: &mut H) {
            match self {
                PathKey::Stdout => 0u8.hash(state),
                PathKey::File(path) => {
                    1u8.hash(state);
                    path.hash(state);
                }
            }
        }
    }

    struct SharedWriter {
        tx: Sender<Action>,
        thread: Mutex<Option<JoinHandle<()>>>,
    }

    impl SharedWriter {
        fn new(path: Option<String>, unix_ts: bool) -> Self {
            let (tx, rx) = crossbeam_channel::bounded(CHANNEL_CAPACITY);
            let ctx = Context {
                rx,
                path,
                date: Local::now().date_naive(),
                unix_ts,
            };
            let thread = std::thread::spawn(move || {
                if let Err(msg) = worker(ctx) {
                    eprintln!("error {}", msg);
                }
            });

            SharedWriter {
                tx,
                thread: Mutex::new(Some(thread)),
            }
        }

        fn stop(&self) {
            let mut thread = self.thread.lock().unwrap();
            if let Some(thread) = thread.take() {
                let _ = self.tx.send(Action::Exit);
                let _ = thread.join();
            }
        }
    }

    impl Drop for SharedWriter {
        fn drop(&mut self) {
            self.stop();
        }
    }

    fn registry() -> &'static Mutex<HashMap<PathKey, Weak<SharedWriter>>> {
        static REGISTRY: OnceLock<Mutex<HashMap<PathKey, Weak<SharedWriter>>>> = OnceLock::new();
        REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
    }

    fn default_path_cell() -> &'static OnceLock<Mutex<Option<String>>> {
        static DEFAULT_PATH: OnceLock<Mutex<Option<String>>> = OnceLock::new();
        &DEFAULT_PATH
    }

    fn default_unix_ts_cell() -> &'static OnceLock<Mutex<bool>> {
        static DEFAULT_UNIX_TS: OnceLock<Mutex<bool>> = OnceLock::new();
        &DEFAULT_UNIX_TS
    }

    fn default_path() -> Option<String> {
        default_path_cell()
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap()
            .clone()
    }

    fn default_unix_ts() -> bool {
        *default_unix_ts_cell()
            .get_or_init(|| Mutex::new(false))
            .lock()
            .unwrap()
    }

    fn set_default_path(path: Option<String>) {
        let cell = default_path_cell().get_or_init(|| Mutex::new(None));
        *cell.lock().unwrap() = path;
    }

    fn set_default_unix_ts(unix_ts: bool) {
        let cell = default_unix_ts_cell().get_or_init(|| Mutex::new(false));
        *cell.lock().unwrap() = unix_ts;
    }

    fn shared_writer(path: Option<String>) -> Arc<SharedWriter> {
        let key = match path.clone() {
            Some(p) => PathKey::File(p),
            None => PathKey::Stdout,
        };

        let mut map = registry().lock().unwrap();
        if let Some(weak) = map.get(&key) {
            if let Some(writer) = weak.upgrade() {
                return writer;
            }
        }

        let writer = Arc::new(SharedWriter::new(path, default_unix_ts()));
        map.insert(key, Arc::downgrade(&writer));
        writer
    }

    fn level_to_u8(level: log::Level) -> u8 {
        match level {
            log::Level::Error => 1,
            log::Level::Warn => 2,
            log::Level::Info => 3,
            log::Level::Debug => 4,
            log::Level::Trace => 5,
        }
    }

    #[pyclass]
    #[derive(Clone, Copy)]
    pub enum PyLevel {
        Trace,
        Debug,
        Info,
        Warn,
        Error,
    }

    impl From<PyLevel> for LevelFilter {
        fn from(level: PyLevel) -> Self {
            match level {
                PyLevel::Trace => LevelFilter::Trace,
                PyLevel::Debug => LevelFilter::Debug,
                PyLevel::Info => LevelFilter::Info,
                PyLevel::Warn => LevelFilter::Warn,
                PyLevel::Error => LevelFilter::Error,
            }
        }
    }

    impl From<PyLevel> for log::Level {
        fn from(level: PyLevel) -> Self {
            match level {
                PyLevel::Trace => log::Level::Trace,
                PyLevel::Debug => log::Level::Debug,
                PyLevel::Info => log::Level::Info,
                PyLevel::Warn => log::Level::Warn,
                PyLevel::Error => log::Level::Error,
            }
        }
    }

    #[pyclass]
    pub struct PyLogger {
        writer: Arc<SharedWriter>,
        name: Option<Arc<str>>,
        level: AtomicU8,
    }

    #[pymethods]
    impl PyLogger {
        #[new]
        #[pyo3(signature = (name, path=None, level=PyLevel::Info))]
        fn new(name: Option<String>, path: Option<String>, level: PyLevel) -> PyResult<Self> {
            Ok(PyLogger {
                writer: shared_writer(path),
                name: name.map(Arc::from),
                level: AtomicU8::new(level_to_u8(level.into())),
            })
        }

        fn shutdown(&self) {
            flush_thread_buffer(&self.writer.tx);
            let _ = self.writer.tx.send(Action::Flush);
            if Arc::strong_count(&self.writer) == 1 {
                self.writer.stop();
            }
        }

        fn trace(&self, message: &str) {
            self.log_internal(log::Level::Trace, message);
        }

        fn debug(&self, message: &str) {
            self.log_internal(log::Level::Debug, message);
        }

        fn info(&self, message: &str) {
            self.log_internal(log::Level::Info, message);
        }

        fn warn(&self, message: &str) {
            self.log_internal(log::Level::Warn, message);
        }

        fn error(&self, message: &str) {
            self.log_internal(log::Level::Error, message);
        }
    }

    impl PyLogger {
        #[inline]
        fn log_internal(&self, level: log::Level, message: &str) {
            let max_level = self.level.load(Ordering::Relaxed);
            if level_to_u8(level) <= max_level {
                let msg = {
                    let mut inline = ArrayString::<INLINE_MSG_CAP>::new();
                    if inline.try_push_str(message).is_ok() {
                        LogMessage::Inline(inline)
                    } else {
                        LogMessage::Heap(message.to_owned())
                    }
                };
                let entry = LogEntry {
                    ts: cached_timestamp(),
                    name: self.name.as_ref().map(Arc::clone),
                    level,
                    msg,
                };
                push_entry(&self.writer.tx, entry);
            }
        }
    }

    #[pymodule]
    #[pyo3(name = "_logger")]
    pub fn logger_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
        #[pyfunction]
        #[pyo3(signature = (path=None, unix_ts=false))]
        fn basic_config(path: Option<String>, unix_ts: bool) -> PyResult<()> {
            set_default_path(path);
            set_default_unix_ts(unix_ts);
            Ok(())
        }

        #[pyfunction]
        #[pyo3(signature = (name, level=PyLevel::Info))]
        fn get_logger(name: Option<String>, level: PyLevel) -> PyResult<PyLogger> {
            Ok(PyLogger {
                writer: shared_writer(default_path()),
                name: name.map(Arc::from),
                level: AtomicU8::new(level_to_u8(level.into())),
            })
        }

        m.add_class::<PyLevel>()?;
        m.add_class::<PyLogger>()?;
        m.add_function(wrap_pyfunction!(basic_config, m)?)?;
        m.add_function(wrap_pyfunction!(get_logger, m)?)?;
        Ok(())
    }
}
