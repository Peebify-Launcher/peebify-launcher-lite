// ------------ Playtime Tracker ------------
// Counts how long each game has been open and saves it as sessions, with a checkpoint so a crash does not lose them.
// Also builds the totals and session history the playtime page shows. Sessions under 30 seconds are ignored.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};

use parking_lot::Mutex;
use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

use super::BackendState;
use super::game_profiles;

pub(crate) const MAX_SESSIONS: usize = 1000;
const MIN_SESSION_MS: i64 = 30_000;
const RECENT_SESSIONS: usize = 400;
const TICK_CAP_MS: i64 = 30_000;
const CHECKPOINT_INTERVAL_MS: i64 = 60_000;
const OPEN_SESSIONS_FILE: &str = "active-sessions.json";

fn counts_as_session(duration_ms: i64) -> bool {
    duration_ms >= MIN_SESSION_MS
}

fn current_date_key() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

fn days_ago_key(days: i64) -> String {
    let today = chrono::Local::now().date_naive();
    today
        .checked_sub_days(chrono::Days::new(days.max(0) as u64))
        .unwrap_or(today)
        .format("%Y-%m-%d")
        .to_string()
}

pub(super) fn local_day_key(ms: i64) -> Option<String> {
    use chrono::TimeZone;
    match chrono::Local.timestamp_millis_opt(ms) {
        chrono::LocalResult::Single(dt) | chrono::LocalResult::Ambiguous(dt, _) => {
            Some(dt.format("%Y-%m-%d").to_string())
        }
        chrono::LocalResult::None => None,
    }
}

pub(super) struct ProcessIdentity {
    pub(super) image: String,
    pub(super) created_ms: i64,
}

pub(super) struct ActiveSession {
    pub(super) start_ms: i64,
    pub(super) game_id: String,
    pub(super) proc_name: String,
    pub(super) pid: Option<u32>,
    pub(super) created_ms: Option<i64>,
    active_ms: i64,
    last_tick_ms: i64,
    days: BTreeMap<String, i64>,
}

impl ActiveSession {
    fn new(game_id: String, proc_name: String, start_ms: i64) -> Self {
        Self {
            start_ms,
            game_id,
            proc_name,
            pid: None,
            created_ms: None,
            active_ms: 0,
            last_tick_ms: start_ms,
            days: BTreeMap::new(),
        }
    }

    pub(super) fn resumable_pid(
        &self,
        by_name: Option<u32>,
        identify: impl Fn(u32) -> Option<ProcessIdentity>,
    ) -> Option<u32> {
        let same_image = |id: &ProcessIdentity| id.image.eq_ignore_ascii_case(&self.proc_name);
        let ran_then = |id: &ProcessIdentity| id.created_ms <= self.last_tick_ms;
        match (self.pid, self.created_ms) {
            (Some(pid), Some(created_ms)) => identify(pid)
                .filter(|id| same_image(id) && id.created_ms == created_ms)
                .map(|_| pid),
            (Some(pid), None) => match identify(pid) {
                Some(id) => (same_image(&id) && ran_then(&id)).then_some(pid),
                None => (by_name == Some(pid)).then_some(pid),
            },
            (None, _) => {
                let pid = by_name?;
                identify(pid)
                    .filter(|id| same_image(id) && ran_then(id))
                    .map(|_| pid)
            }
        }
    }

    fn advance(&mut self, now_ms: i64, day: &str) {
        let delta = (now_ms - self.last_tick_ms).clamp(0, TICK_CAP_MS);
        self.last_tick_ms = now_ms;
        if delta == 0 {
            return;
        }
        self.active_ms += delta;
        *self.days.entry(day.to_string()).or_insert(0) += delta;
    }

    fn advance_to_now(&mut self) {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let day = local_day_key(now_ms).unwrap_or_else(current_date_key);
        self.advance(now_ms, &day);
    }

    fn to_json(&self) -> Value {
        let mut value = json!({
            "gameId": self.game_id,
            "procName": self.proc_name,
            "startMs": self.start_ms,
            "activeMs": self.active_ms,
            "lastSeenMs": self.last_tick_ms,
            "days": self.days,
        });
        if let Some(pid) = self.pid {
            value["pid"] = json!(pid);
        }
        if let Some(created_ms) = self.created_ms {
            value["createdMs"] = json!(created_ms);
        }
        value
    }

    fn from_json(v: &Value) -> Option<Self> {
        let game_id = v.get("gameId")?.as_str()?.to_string();
        let start_ms = v.get("startMs")?.as_i64().filter(|ms| *ms > 0)?;
        if game_id.is_empty() || game_id.contains('.') {
            return None;
        }
        let days = v
            .get("days")
            .and_then(Value::as_object)
            .map(|map| {
                map.iter()
                    .filter_map(|(day, ms)| Some((day.clone(), ms.as_i64()?.max(0))))
                    .collect()
            })
            .unwrap_or_default();
        Some(Self {
            start_ms,
            proc_name: v
                .get("procName")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            pid: v
                .get("pid")
                .and_then(Value::as_u64)
                .and_then(|pid| u32::try_from(pid).ok())
                .filter(|pid| *pid != 0),
            created_ms: v
                .get("createdMs")
                .and_then(Value::as_i64)
                .filter(|ms| *ms > 0),
            active_ms: v.get("activeMs").and_then(Value::as_i64).unwrap_or(0).max(0),
            last_tick_ms: v
                .get("lastSeenMs")
                .and_then(Value::as_i64)
                .unwrap_or(start_ms),
            days,
            game_id,
        })
    }
}

fn record_session(
    playtime: &mut serde_json::Map<String, Value>,
    session: &ActiveSession,
    end_ms: i64,
) -> Option<f64> {
    let num = |v: Option<&Value>| v.and_then(Value::as_f64).unwrap_or(0.0);
    let duration_ms = session.active_ms.max(0);
    let mut sessions = playtime
        .get("sessions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if sessions
        .iter()
        .any(|s| s.get("startTime").and_then(Value::as_i64) == Some(session.start_ms))
    {
        return None;
    }

    let new_total = num(playtime.get("totalPlaytime")) + duration_ms as f64;
    let mut daily = playtime
        .get("dailyPlaytime")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for (day, ms) in session.days.iter().filter(|(_, ms)| **ms > 0) {
        let day_total = num(daily.get(day)) + *ms as f64;
        daily.insert(day.clone(), json!(day_total));
    }
    let new_session_count = num(playtime.get("sessionCount")) + 1.0;

    let mut row = json!({
        "gameId": session.game_id,
        "startTime": session.start_ms,
        "endTime": end_ms.max(session.start_ms + duration_ms),
        "duration": duration_ms,
    });
    if session.days.len() > 1 {
        row["days"] = json!(session.days);
    }
    sessions.push(row);
    if sessions.len() > MAX_SESSIONS {
        let excess = sessions.len() - MAX_SESSIONS;
        sessions.drain(0..excess);
    }

    playtime.insert("totalPlaytime".to_string(), json!(new_total));
    playtime.insert("dailyPlaytime".to_string(), Value::Object(daily));
    playtime.insert(
        "mostRecentSession".to_string(),
        json!({ "startTime": session.start_ms, "duration": duration_ms }),
    );
    playtime.insert("sessionCount".to_string(), json!(new_session_count));
    playtime.insert("sessions".to_string(), Value::Array(sessions));
    Some(new_total)
}

struct LiveSession {
    game_id: String,
    start_ms: i64,
    active_ms: i64,
    last_ms: i64,
    days: BTreeMap<String, i64>,
}

pub struct PlaytimeTracker {
    sessions: Mutex<HashMap<String, ActiveSession>>,
    store: Option<PathBuf>,
    store_lock: Mutex<()>,
    last_checkpoint_ms: AtomicI64,
}

impl PlaytimeTracker {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            store: super::user_data_dir_before_setup()
                .map(|dir| dir.join(OPEN_SESSIONS_FILE)),
            store_lock: Mutex::new(()),
            last_checkpoint_ms: AtomicI64::new(0),
        }
    }

    pub fn start_tracking(&self, game_id: &str, proc_name: &str) {
        let start_ms = chrono::Utc::now().timestamp_millis();
        log::info!("Playtime tracking started for {game_id}");
        self.sessions.lock().insert(
            game_id.to_string(),
            ActiveSession::new(game_id.to_string(), proc_name.to_string(), start_ms),
        );
        self.checkpoint();
    }

    pub(super) fn set_process(&self, game_id: &str, pid: u32, created_ms: Option<i64>) {
        {
            let mut sessions = self.sessions.lock();
            let Some(session) = sessions.get_mut(game_id) else {
                return;
            };
            if session.pid == Some(pid) && session.created_ms == created_ms {
                return;
            }
            session.pid = Some(pid);
            session.created_ms = created_ms;
        }
        self.checkpoint();
    }

    pub fn tick_holding(&self, held: &[String]) {
        let now_ms = chrono::Utc::now().timestamp_millis();
        {
            let mut sessions = self.sessions.lock();
            if sessions.is_empty() {
                return;
            }
            let day = local_day_key(now_ms).unwrap_or_else(current_date_key);
            for (game_id, session) in sessions.iter_mut() {
                if !held.contains(game_id) {
                    session.advance(now_ms, &day);
                }
            }
        }
        let last = self.last_checkpoint_ms.load(Ordering::SeqCst);
        if now_ms < last || now_ms - last >= CHECKPOINT_INTERVAL_MS {
            self.checkpoint();
        }
    }

    pub fn stop_tracking_at(
        &self,
        app: &AppHandle,
        game_id: &str,
        ended_ms: i64,
    ) -> Option<i64> {
        let mut session = self.sessions.lock().remove(game_id)?;
        let end_ms = ended_ms.min(chrono::Utc::now().timestamp_millis());
        let day = local_day_key(end_ms).unwrap_or_else(current_date_key);
        session.advance(end_ms, &day);
        log::info!(
            "Playtime session ended ({game_id}). Active duration: {}s",
            session.active_ms / 1000
        );
        Self::save_session_data(app, &session, end_ms);
        self.checkpoint();
        Some(session.active_ms)
    }

    fn live_sessions(&self) -> Vec<LiveSession> {
        self.sessions
            .lock()
            .values()
            .filter(|s| counts_as_session(s.active_ms))
            .map(|s| LiveSession {
                game_id: s.game_id.clone(),
                start_ms: s.start_ms,
                active_ms: s.active_ms,
                last_ms: s.last_tick_ms.max(s.start_ms),
                days: s.days.clone(),
            })
            .collect()
    }

    pub fn flush_all(&self, app: &AppHandle) {
        let drained: Vec<ActiveSession> = self.sessions.lock().drain().map(|(_, s)| s).collect();
        if drained.is_empty() {
            return;
        }
        let now_ms = chrono::Utc::now().timestamp_millis();
        for mut session in drained {
            session.advance_to_now();
            log::info!(
                "Playtime session for {} saved on exit. Active duration: {}s",
                session.game_id,
                session.active_ms / 1000
            );
            Self::save_session_data(app, &session, now_ms);
        }
        self.checkpoint();
    }

    pub(super) fn take_leftovers(&self) -> Vec<ActiveSession> {
        let Some(path) = self.store.as_ref() else {
            return Vec::new();
        };
        let _guard = self.store_lock.lock();
        let raw = match std::fs::read(path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(e) => {
                log::warn!("Could not read open playtime sessions: {e}");
                return Vec::new();
            }
        };
        serde_json::from_slice::<Value>(&raw)
            .ok()
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(ActiveSession::from_json)
            .collect()
    }

    pub(super) fn adopt(&self, mut session: ActiveSession) {
        session.last_tick_ms = chrono::Utc::now().timestamp_millis();
        log::info!(
            "Playtime session for {} resumed after a restart ({}s so far)",
            session.game_id,
            session.active_ms / 1000
        );
        self.sessions
            .lock()
            .entry(session.game_id.clone())
            .or_insert(session);
    }

    pub(super) fn close_leftover(&self, app: &AppHandle, session: &ActiveSession) {
        log::info!(
            "Playtime session for {} was left open by the last run. Closing it at its last checkpoint ({}s).",
            session.game_id,
            session.active_ms / 1000
        );
        Self::save_session_data(app, session, session.last_tick_ms);
    }

    pub fn checkpoint(&self) {
        let Some(path) = self.store.as_ref() else {
            return;
        };
        let now_ms = chrono::Utc::now().timestamp_millis();
        self.last_checkpoint_ms.store(now_ms, Ordering::SeqCst);
        let open: Vec<Value> = self
            .sessions
            .lock()
            .values()
            .map(ActiveSession::to_json)
            .collect();
        let _guard = self.store_lock.lock();
        if open.is_empty() {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => log::warn!("Could not clear open playtime sessions: {e}"),
            }
            return;
        }
        let tmp = path.with_extension("json.tmp");
        let written = std::fs::write(&tmp, Value::Array(open).to_string())
            .map_err(|e| e.to_string())
            .and_then(|()| super::fs_util::finalize_replace(&tmp, path));
        if let Err(e) = written {
            log::warn!("Could not save open playtime sessions: {e}");
        }
    }

    fn save_session_data(app: &AppHandle, session: &ActiveSession, end_ms: i64) {
        let config = app.state::<BackendState>().config.clone();
        let game_id = &session.game_id;
        if !counts_as_session(session.active_ms) {
            log::info!(
                "Playtime session for {game_id} not recorded: {}s is under the {}s minimum.",
                session.active_ms / 1000,
                MIN_SESSION_MS / 1000
            );
            return;
        }

        let mut new_total = None;
        config.update(&format!("games.{game_id}.playtime"), |prev| {
            let mut playtime = prev.as_object().cloned().unwrap_or_default();
            new_total = record_session(&mut playtime, session, end_ms);
            if new_total.is_none() {
                return false;
            }
            *prev = Value::Object(playtime);
            true
        });
        let Some(new_total) = new_total else {
            log::info!(
                "Playtime session for {game_id} started at {} is already recorded.",
                session.start_ms
            );
            return;
        };
        config.flush();
        log::info!(
            "Saved session ({game_id}). New total playtime: {} minutes.",
            (new_total / 1000.0 / 60.0).round()
        );
    }
}

fn iso_utc(ms: i64) -> Option<String> {
    use chrono::TimeZone;
    match chrono::Utc.timestamp_millis_opt(ms) {
        chrono::LocalResult::Single(dt) => {
            Some(dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        }
        _ => None,
    }
}

struct GameTotals {
    total_minutes: f64,
    sessions: f64,
    last_played_ms: Option<i64>,
}

fn is_recorded(games_cfg: &Value, game_id: &str, start: i64) -> bool {
    games_cfg
        .get(game_id)
        .and_then(|g| g.get("playtime"))
        .and_then(|p| p.get("sessions"))
        .and_then(Value::as_array)
        .is_some_and(|rows| {
            rows.iter()
                .any(|s| s.get("startTime").and_then(Value::as_i64) == Some(start))
        })
}

fn session_end(row: &Value, start: i64, duration_ms: f64) -> i64 {
    row.get("endTime")
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite())
        .map(|v| v as i64)
        .filter(|end| *end >= start)
        .unwrap_or(start + duration_ms.max(0.0) as i64)
}

fn collect(
    games_cfg: &Value,
    live: &[LiveSession],
) -> (
    HashMap<String, GameTotals>,
    HashMap<String, HashMap<String, f64>>,
) {
    let mut games: HashMap<String, GameTotals> = HashMap::new();
    let mut daily: HashMap<String, HashMap<String, f64>> = HashMap::new();

    let mut ids: Vec<&str> = games_cfg
        .as_object()
        .map(|map| map.keys().map(String::as_str).collect())
        .unwrap_or_default();
    for session in live {
        if !ids.contains(&session.game_id.as_str()) {
            ids.push(&session.game_id);
        }
    }
    for id in ids {
        let pt = games_cfg
            .get(id)
            .and_then(|g| g.get("playtime"))
            .unwrap_or(&Value::Null);
        let open: Vec<&LiveSession> = live.iter().filter(|s| s.game_id == id).collect();
        let total = pt
            .get("totalPlaytime")
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
            + open.iter().map(|s| s.active_ms as f64).sum::<f64>();
        let sessions = pt
            .get("sessionCount")
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
            + open.len() as f64;
        let last = pt.get("mostRecentSession").and_then(|s| {
            let start = s.get("startTime").and_then(Value::as_i64)?;
            let duration = s.get("duration").and_then(Value::as_f64).unwrap_or(0.0);
            Some(session_end(s, start, duration))
        });
        games.insert(
            id.to_string(),
            GameTotals {
                total_minutes: (total / 60000.0).round(),
                sessions,
                last_played_ms: last,
            },
        );
        let mut day_ms: HashMap<&str, f64> = HashMap::new();
        if let Some(day_map) = pt.get("dailyPlaytime").and_then(|v| v.as_object()) {
            for (date, ms) in day_map {
                *day_ms.entry(date).or_insert(0.0) += ms.as_f64().unwrap_or(0.0);
            }
        }
        for session in &open {
            for (date, ms) in &session.days {
                *day_ms.entry(date).or_insert(0.0) += *ms as f64;
            }
        }
        for (date, ms) in day_ms {
            daily
                .entry(date.to_string())
                .or_default()
                .insert(id.to_string(), (ms / 60000.0).round());
        }
    }

    (games, daily)
}

struct SessionRow {
    start: i64,
    end: i64,
    game_id: String,
    minutes: f64,
    local: bool,
    live: bool,
}

fn session_rows(
    games_cfg: &Value,
    live: &[LiveSession],
) -> (HashMap<String, f64>, Vec<SessionRow>) {
    let mut rows: Vec<SessionRow> = Vec::new();
    let mut local_keys: HashSet<(String, i64)> = HashSet::new();
    if let Some(map) = games_cfg.as_object() {
        for (id, g) in map {
            let Some(arr) = g
                .get("playtime")
                .and_then(|p| p.get("sessions"))
                .and_then(Value::as_array)
            else {
                continue;
            };
            for s in arr {
                let start = s.get("startTime").and_then(Value::as_i64).unwrap_or(0);
                let duration = s.get("duration").and_then(Value::as_f64).unwrap_or(0.0);
                let min = duration / 60000.0;
                if start <= 0 || min <= 0.0 {
                    continue;
                }
                local_keys.insert((id.clone(), start));
                rows.push(SessionRow {
                    start,
                    end: session_end(s, start, duration),
                    game_id: id.clone(),
                    minutes: min,
                    local: true,
                    live: false,
                });
            }
        }
    }

    for session in live {
        if !local_keys.insert((session.game_id.clone(), session.start_ms)) {
            continue;
        }
        rows.push(SessionRow {
            start: session.start_ms,
            end: session.last_ms,
            game_id: session.game_id.clone(),
            minutes: session.active_ms as f64 / 60000.0,
            local: false,
            live: true,
        });
    }

    let mut longest: HashMap<String, f64> = HashMap::new();
    for row in &rows {
        let e = longest.entry(row.game_id.clone()).or_insert(0.0);
        *e = e.max(row.minutes);
    }

    rows.sort_by_key(|row| std::cmp::Reverse(row.start));
    (longest, rows)
}

fn session_json(row: &SessionRow) -> Value {
    let mut value = json!({
        "gameId": row.game_id,
        "start": row.start,
        "end": row.end,
        "minutes": (row.minutes * 10.0).round() / 10.0,
        "local": row.local,
    });
    if row.live {
        value["live"] = json!(true);
    }
    value
}

fn last_played(rows: &[SessionRow], games: &HashMap<String, GameTotals>) -> HashMap<String, i64> {
    let mut out: HashMap<String, i64> = games
        .iter()
        .filter_map(|(id, g)| Some((id.clone(), g.last_played_ms?)))
        .collect();
    for row in rows {
        let at = out.entry(row.game_id.clone()).or_insert(row.end);
        *at = (*at).max(row.end);
    }
    out
}

fn session_page(rows: &[SessionRow], before: i64, limit: usize) -> (&[SessionRow], bool) {
    let rest = &rows[rows.partition_point(|row| row.start >= before)..];
    let n = limit.min(rest.len());
    (&rest[..n], rest.len() > n)
}

pub(super) async fn get_playtime_sessions(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let before = args
        .first()
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite())
        .map_or(i64::MAX, |v| v as i64);
    let config = app.state::<BackendState>().config.clone();
    let (_, rows) = session_rows(&config.get("games"), &[]);
    let (page, has_more) = session_page(&rows, before, RECENT_SESSIONS);
    Ok(super::ok_with(json!({
        "sessions": page.iter().map(session_json).collect::<Vec<_>>(),
        "hasMore": has_more,
    })))
}

fn remove_session(playtime: &mut serde_json::Map<String, Value>, start: i64) -> Option<f64> {
    let num = |v: Option<&Value>| v.and_then(Value::as_f64).unwrap_or(0.0);
    let start_of = |s: &Value| s.get("startTime").and_then(Value::as_i64);

    let sessions = playtime.get_mut("sessions")?.as_array_mut()?;
    let index = sessions.iter().position(|s| start_of(s) == Some(start))?;
    let removed = sessions.remove(index);
    let latest = sessions
        .iter()
        .max_by_key(|s| start_of(s).unwrap_or(0))
        .map(|s| json!({ "startTime": s.get("startTime"), "duration": s.get("duration") }));

    let duration = num(removed.get("duration")).max(0.0);
    let end = removed
        .get("endTime")
        .and_then(Value::as_i64)
        .unwrap_or(start + duration as i64);

    let total = (num(playtime.get("totalPlaytime")) - duration).max(0.0);
    playtime.insert("totalPlaytime".to_string(), json!(total));
    let count = (num(playtime.get("sessionCount")) - 1.0).max(0.0);
    playtime.insert("sessionCount".to_string(), json!(count));

    let slices: Vec<(String, f64)> = match removed.get("days").and_then(Value::as_object) {
        Some(days) => days
            .iter()
            .map(|(day, ms)| (day.clone(), ms.as_f64().unwrap_or(0.0).max(0.0)))
            .collect(),
        None => local_day_key(end).map(|day| (day, duration)).into_iter().collect(),
    };
    if let Some(daily) = playtime
        .get_mut("dailyPlaytime")
        .and_then(Value::as_object_mut)
    {
        for (day, slice) in slices {
            if let Some(ms) = daily.get(&day).and_then(Value::as_f64) {
                let left = (ms - slice).max(0.0);
                if left > 0.0 {
                    daily.insert(day, json!(left));
                } else {
                    daily.remove(&day);
                }
            }
        }
    }

    let recent_was_removed = playtime
        .get("mostRecentSession")
        .and_then(start_of)
        .is_some_and(|at| at == start);
    if recent_was_removed {
        match latest {
            Some(latest) => {
                playtime.insert("mostRecentSession".to_string(), latest);
            }
            None => {
                playtime.remove("mostRecentSession");
            }
        }
    }

    Some(duration)
}

pub(super) async fn delete_session(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let game_id = super::arg_str(args, 0).unwrap_or_default().to_string();
    let start = args
        .get(1)
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite())
        .map_or(0, |v| v as i64);
    if game_id.is_empty() || game_id.contains('.') || start <= 0 {
        return Ok(super::err_response("That session could not be found."));
    }
    let state = app.state::<BackendState>();
    let config = state.config.clone();
    let key = format!("games.{game_id}.playtime");
    let mut removed = None;
    config.update(&key, |playtime| {
        removed = playtime
            .as_object_mut()
            .and_then(|playtime| remove_session(playtime, start));
        removed.is_some()
    });
    let Some(removed_ms) = removed else {
        return Ok(super::err_response(
            "That session could not be found. Only sessions recorded on this PC can be removed.",
        ));
    };
    config.flush();
    log::info!(
        "Removed playtime session ({game_id}) started at {start}, {}s long.",
        (removed_ms / 1000.0).round()
    );
    Ok(super::ok_response())
}

pub(super) async fn get_playtime_data(app: &AppHandle) -> Result<Value, String> {
    let state = app.state::<BackendState>();
    let config = state.config.clone();
    let games_cfg = config.get("games");
    let live: Vec<LiveSession> = state
        .game
        .playtime
        .live_sessions()
        .into_iter()
        .filter(|s| !is_recorded(&games_cfg, &s.game_id, s.start_ms))
        .collect();
    let (games, daily) = collect(&games_cfg, &live);
    let (longest, rows) = session_rows(&games_cfg, &live);
    let last = last_played(&rows, &games);
    let last_iso = |id: &str| last.get(id).copied().and_then(iso_utc);

    let mut pill = serde_json::Map::new();
    let mut ids: Vec<&str> = game_profiles::GAME_IDS.to_vec();
    for id in games.keys() {
        if !ids.contains(&id.as_str()) {
            ids.push(id);
        }
    }
    for id in &ids {
        let g = games.get(*id);
        pill.insert(
            id.to_string(),
            json!({
                "allTimeMinutes": g.map_or(0.0, |g| g.total_minutes),
                "sessions": g.map_or(0.0, |g| g.sessions),
                "lastPlayed": last_iso(id),
            }),
        );
    }

    let sessions_total = rows.len();
    let recent_sessions: Vec<Value> = rows.iter().take(RECENT_SESSIONS).map(session_json).collect();

    let game_list: Vec<Value> = ids
        .iter()
        .map(|id| {
            let g = games.get(*id);
            let ls = longest.get(*id);
            json!({
                "id": id,
                "minutes": g.map_or(0.0, |g| g.total_minutes),
                "sessions": g.map_or(0.0, |g| g.sessions) as i64,
                "longestSessionMinutes": ls.map_or(0.0, |l| (l * 10.0).round() / 10.0),
            })
        })
        .collect();

    let cutoff = days_ago_key(372);
    let mut daily_out = serde_json::Map::new();
    for (date, per_game) in &daily {
        if date.as_str() < cutoff.as_str() {
            continue;
        }
        let mut m = serde_json::Map::new();
        for (gid, min) in per_game {
            if *min > 0.0 {
                m.insert(gid.clone(), json!(min));
            }
        }
        if !m.is_empty() {
            daily_out.insert(date.clone(), Value::Object(m));
        }
    }

    Ok(super::ok_with(json!({
        "pill": pill,
        "dashboard": {
            "games": game_list,
            "daily": Value::Object(daily_out),
            "recentSessions": recent_sessions,
            "sessionsTruncated": sessions_total > RECENT_SESSIONS,
        },
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_playtime() -> serde_json::Map<String, Value> {
        let end_a = 1_700_000_000_000i64 + 3_600_000;
        let end_b = 1_700_100_000_000i64 + 32_400_000;
        let mut daily = serde_json::Map::new();
        daily.insert(local_day_key(end_a).unwrap(), json!(3_600_000.0));
        daily.insert(local_day_key(end_b).unwrap(), json!(33_000_000.0));
        json!({
            "totalPlaytime": 36_600_000.0,
            "sessionCount": 3.0,
            "dailyPlaytime": Value::Object(daily),
            "mostRecentSession": { "startTime": 1_700_100_000_000i64, "duration": 32_400_000 },
            "sessions": [
                { "gameId": "wuwa", "startTime": 1_700_000_000_000i64, "endTime": end_a, "duration": 3_600_000 },
                { "gameId": "wuwa", "startTime": 1_700_099_000_000i64, "endTime": 1_700_099_600_000i64, "duration": 600_000 },
                { "gameId": "wuwa", "startTime": 1_700_100_000_000i64, "endTime": end_b, "duration": 32_400_000 },
            ],
        })
        .as_object()
        .cloned()
        .unwrap()
    }

    #[test]
    fn removing_a_session_takes_it_out_of_every_total() {
        let mut pt = sample_playtime();
        let day_b = local_day_key(1_700_100_000_000i64 + 32_400_000).unwrap();
        assert_eq!(remove_session(&mut pt, 1_700_100_000_000), Some(32_400_000.0));
        assert_eq!(pt["totalPlaytime"], json!(4_200_000.0));
        assert_eq!(pt["sessionCount"], json!(2.0));
        assert_eq!(pt["sessions"].as_array().unwrap().len(), 2);
        assert_eq!(pt["dailyPlaytime"][&day_b].as_f64(), Some(600_000.0));
        assert_eq!(pt["mostRecentSession"]["startTime"], json!(1_700_099_000_000i64));
    }

    #[test]
    fn removing_the_last_minutes_of_a_day_drops_the_day_and_totals_stay_non_negative() {
        let mut pt = sample_playtime();
        pt.insert("totalPlaytime".to_string(), json!(1_000.0));
        pt.insert("sessionCount".to_string(), json!(0.0));
        let day_a = local_day_key(1_700_000_000_000i64 + 3_600_000).unwrap();
        assert!(remove_session(&mut pt, 1_700_000_000_000).is_some());
        assert_eq!(pt["totalPlaytime"], json!(0.0));
        assert_eq!(pt["sessionCount"], json!(0.0));
        assert!(pt["dailyPlaytime"].get(&day_a).is_none());
        assert_eq!(pt["mostRecentSession"]["startTime"], json!(1_700_100_000_000i64));
    }

    #[test]
    fn removing_the_only_session_clears_the_most_recent_marker() {
        let mut pt = json!({
            "totalPlaytime": 60_000.0,
            "sessionCount": 1.0,
            "mostRecentSession": { "startTime": 1_700_000_000_000i64, "duration": 60_000 },
            "sessions": [{ "startTime": 1_700_000_000_000i64, "endTime": 1_700_000_060_000i64, "duration": 60_000 }],
        })
        .as_object()
        .cloned()
        .unwrap();
        assert!(remove_session(&mut pt, 1_700_000_000_000).is_some());
        assert!(pt.get("mostRecentSession").is_none());
    }

    #[test]
    fn an_unknown_session_changes_nothing() {
        let mut pt = sample_playtime();
        let before = pt.clone();
        assert!(remove_session(&mut pt, 42).is_none());
        assert_eq!(pt, before);
    }

    #[test]
    fn sessions_under_thirty_seconds_are_not_recorded() {
        assert!(!counts_as_session(10_000));
        assert!(!counts_as_session(29_999));
        assert!(counts_as_session(30_000));
        assert!(counts_as_session(3_600_000));
    }

    fn rows(starts: &[i64]) -> Vec<SessionRow> {
        starts
            .iter()
            .map(|&start| SessionRow {
                start,
                end: start + 60_000,
                game_id: "wuwa".to_string(),
                minutes: 1.0,
                local: true,
                live: false,
            })
            .collect()
    }

    #[test]
    fn a_session_page_starts_strictly_before_the_cursor() {
        let all = rows(&[500, 400, 300, 200, 100]);
        let (page, more) = session_page(&all, 400, 2);
        assert_eq!(page.iter().map(|r| r.start).collect::<Vec<_>>(), vec![300, 200]);
        assert!(more);
        let (page, more) = session_page(&all, 200, 5);
        assert_eq!(page.iter().map(|r| r.start).collect::<Vec<_>>(), vec![100]);
        assert!(!more);
        let (page, more) = session_page(&all, 50, 5);
        assert!(page.is_empty());
        assert!(!more);
    }

    fn live(game_id: &str, start_ms: i64, active_ms: i64, days: &[(&str, i64)]) -> LiveSession {
        LiveSession {
            game_id: game_id.to_string(),
            start_ms,
            active_ms,
            last_ms: start_ms + active_ms,
            days: days.iter().map(|(d, ms)| ((*d).to_string(), *ms)).collect(),
        }
    }

    #[test]
    fn a_session_ends_at_its_recorded_end_or_after_its_active_time() {
        let slept = json!({ "startTime": 1_000_000, "endTime": 40_000_000, "duration": 2_700_000 });
        assert_eq!(session_end(&slept, 1_000_000, 2_700_000.0), 40_000_000);
        let old = json!({ "startTime": 1_000_000, "duration": 2_700_000 });
        assert_eq!(session_end(&old, 1_000_000, 2_700_000.0), 3_700_000);
        let junk = json!({ "startTime": 1_000_000, "endTime": 5, "duration": 60_000 });
        assert_eq!(session_end(&junk, 1_000_000, 60_000.0), 1_060_000);
    }

    #[test]
    fn the_open_session_counts_toward_totals_days_and_sessions() {
        let games = json!({
            "wuwa": { "playtime": {
                "totalPlaytime": 3_600_000.0,
                "sessionCount": 1.0,
                "dailyPlaytime": { "2026-09-24": 3_600_000.0 },
                "sessions": [{ "startTime": 1_000, "endTime": 3_601_000, "duration": 3_600_000 }],
            } },
        });
        let open = [
            live("wuwa", 9_000_000, 7_200_000, &[("2026-09-24", 1_800_000), ("2026-09-25", 5_400_000)]),
            live("zzz", 9_500_000, 600_000, &[("2026-09-25", 600_000)]),
        ];
        let (totals, daily) = collect(&games, &open);
        assert_eq!(totals["wuwa"].total_minutes, 180.0);
        assert_eq!(totals["wuwa"].sessions, 2.0);
        assert_eq!(totals["zzz"].total_minutes, 10.0);
        assert_eq!(daily["2026-09-24"]["wuwa"], 90.0);
        assert_eq!(daily["2026-09-25"]["wuwa"], 90.0);
        assert_eq!(daily["2026-09-25"]["zzz"], 10.0);

        let (_, rows) = session_rows(&games, &open);
        let open_row = rows.iter().find(|r| r.game_id == "wuwa" && r.live).unwrap();
        assert!(!open_row.local, "an open session cannot be removed");
        assert_eq!(session_json(open_row)["live"], json!(true));
        assert!(session_json(&rows[rows.len() - 1]).get("live").is_none());
    }

    #[test]
    fn last_played_is_the_latest_session_end() {
        let games = json!({
            "wuwa": { "playtime": {
                "mostRecentSession": { "startTime": 1_000_000, "duration": 60_000 },
                "sessions": [{ "startTime": 1_000_000, "endTime": 50_000_000, "duration": 60_000 }],
            } },
            "zzz": { "playtime": { "mostRecentSession": { "startTime": 2_000_000, "duration": 60_000 } } },
        });
        let (totals, _) = collect(&games, &[]);
        let (_, rows) = session_rows(&games, &[]);
        let last = last_played(&rows, &totals);
        assert_eq!(last["wuwa"], 50_000_000, "the end, not the start");
        assert_eq!(last["zzz"], 2_060_000, "no rows left, so the saved marker");
        assert_eq!(session_json(&rows[0])["end"], json!(50_000_000));
    }

    #[test]
    fn an_open_session_already_recorded_is_not_listed_twice() {
        let games = json!({ "wuwa": { "playtime": {
            "sessions": [{ "startTime": 1_700_000_000_000i64, "duration": 60_000 }],
        } } });
        let open = [live("wuwa", 1_700_000_000_000, 120_000, &[])];
        let (_, rows) = session_rows(&games, &open);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].local);
    }

    fn open_session(start: i64) -> ActiveSession {
        ActiveSession::new("wuwa".to_string(), "Client-Win64-Shipping.exe".to_string(), start)
    }

    #[test]
    fn active_time_caps_long_gaps_and_ignores_clock_going_back() {
        let mut s = open_session(1_000_000);
        s.advance(1_001_000, "2026-09-24");
        s.advance(1_002_000, "2026-09-24");
        assert_eq!(s.active_ms, 2_000);
        s.advance(1_002_000 + 8 * 3_600_000, "2026-09-24");
        assert_eq!(s.active_ms, 2_000 + TICK_CAP_MS);
        let before = s.active_ms;
        s.advance(500_000, "2026-09-24");
        assert_eq!(s.active_ms, before);
        s.advance(501_000, "2026-09-24");
        assert_eq!(s.active_ms, before + 1_000);
    }

    #[test]
    fn a_session_across_midnight_credits_each_day() {
        let mut s = open_session(1_000_000);
        for i in 1..=60 {
            let day = if i <= 20 { "2026-09-24" } else { "2026-09-25" };
            s.advance(1_000_000 + i * 1_000, day);
        }
        let mut pt = serde_json::Map::new();
        let total = record_session(&mut pt, &s, 1_060_000);
        assert_eq!(total, Some(60_000.0));
        assert_eq!(pt["dailyPlaytime"]["2026-09-24"].as_f64(), Some(20_000.0));
        assert_eq!(pt["dailyPlaytime"]["2026-09-25"].as_f64(), Some(40_000.0));
        assert_eq!(pt["sessions"][0]["duration"], json!(60_000));
        assert_eq!(pt["sessions"][0]["days"]["2026-09-24"], json!(20_000));

        assert_eq!(remove_session(&mut pt, 1_000_000), Some(60_000.0));
        assert!(pt["dailyPlaytime"].as_object().unwrap().is_empty());
        assert_eq!(pt["totalPlaytime"], json!(0.0));
    }

    #[test]
    fn the_same_session_is_never_recorded_twice() {
        let mut s = open_session(1_000_000);
        s.advance(1_030_000, "2026-09-24");
        s.advance(1_060_000, "2026-09-24");
        let mut pt = serde_json::Map::new();
        assert!(record_session(&mut pt, &s, 1_060_000).is_some());
        assert!(record_session(&mut pt, &s, 1_060_000).is_none());
        assert_eq!(pt["sessionCount"], json!(1.0));
        assert_eq!(pt["totalPlaytime"], json!(60_000.0));
        assert!(pt["sessions"][0].get("days").is_none());
    }

    #[test]
    fn an_open_session_survives_the_checkpoint_file() {
        let mut s = open_session(1_000_000);
        s.advance(1_030_000, "2026-09-24");
        s.advance(1_045_000, "2026-09-25");
        let back = ActiveSession::from_json(&s.to_json()).unwrap();
        assert_eq!(back.game_id, "wuwa");
        assert_eq!(back.proc_name, "Client-Win64-Shipping.exe");
        assert_eq!(back.start_ms, 1_000_000);
        assert_eq!(back.active_ms, 45_000);
        assert_eq!(back.last_tick_ms, 1_045_000);
        assert_eq!(back.days, s.days);
        assert_eq!(back.pid, None);
        assert_eq!(back.created_ms, None);
        assert!(ActiveSession::from_json(&json!({ "gameId": "a.b", "startMs": 5 })).is_none());
        assert!(ActiveSession::from_json(&json!({ "gameId": "wuwa", "startMs": 0 })).is_none());
    }

    #[test]
    fn the_tracked_process_survives_the_checkpoint_file() {
        let mut s = open_session(1_000_000);
        s.pid = Some(4242);
        s.created_ms = Some(999_000);
        let saved = s.to_json();
        assert_eq!(saved["pid"], json!(4242));
        let back = ActiveSession::from_json(&saved).unwrap();
        assert_eq!(back.pid, Some(4242));
        assert_eq!(back.created_ms, Some(999_000));
        let old = json!({ "gameId": "wuwa", "startMs": 5, "pid": "x", "createdMs": -1 });
        let back = ActiveSession::from_json(&old).unwrap();
        assert_eq!((back.pid, back.created_ms), (None, None));
    }

    fn running(image: &str, created_ms: i64) -> Option<ProcessIdentity> {
        Some(ProcessIdentity {
            image: image.to_string(),
            created_ms,
        })
    }

    #[test]
    fn only_the_same_run_of_the_game_is_picked_up_again() {
        let mut s = open_session(1_000_000);
        s.advance(1_060_000, "2026-09-24");
        s.pid = Some(4242);
        s.created_ms = Some(999_000);
        let exe = "client-win64-shipping.exe";
        assert_eq!(s.resumable_pid(Some(4242), |_| running(exe, 999_000)), Some(4242));
        assert_eq!(s.resumable_pid(Some(4242), |_| running(exe, 1_200_000)), None);
        assert_eq!(s.resumable_pid(Some(7), |_| running("notepad.exe", 999_000)), None);
        assert_eq!(s.resumable_pid(Some(4242), |_| None), None);
    }

    #[test]
    fn a_session_without_a_creation_time_needs_a_process_that_ran_at_its_checkpoint() {
        let mut s = open_session(1_000_000);
        s.advance(1_060_000, "2026-09-24");
        let exe = "Client-Win64-Shipping.exe";
        assert_eq!(s.resumable_pid(Some(51), |_| running(exe, 990_000)), Some(51));
        assert_eq!(s.resumable_pid(Some(51), |_| running(exe, 1_100_000)), None);
        assert_eq!(s.resumable_pid(None, |_| running(exe, 990_000)), None);
        assert_eq!(s.resumable_pid(Some(51), |_| None), None);
        s.pid = Some(51);
        assert_eq!(s.resumable_pid(Some(51), |_| None), Some(51));
        assert_eq!(s.resumable_pid(Some(52), |_| None), None);
        assert_eq!(s.resumable_pid(Some(51), |_| running(exe, 1_100_000)), None);
    }

    #[test]
    fn day_keys_step_back_by_calendar_day() {
        let today = chrono::Local::now().date_naive();
        assert_eq!(days_ago_key(0), today.format("%Y-%m-%d").to_string());
        assert_eq!(
            days_ago_key(1),
            today.pred_opt().unwrap().format("%Y-%m-%d").to_string()
        );
        assert_eq!(
            days_ago_key(372),
            (today - chrono::Days::new(372)).format("%Y-%m-%d").to_string()
        );
    }
}
