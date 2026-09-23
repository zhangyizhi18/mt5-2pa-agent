//! MT5 桥接客户端：维护 EA 上报的终端状态缓存，并通过指令队列向 EA 下发交易操作。
//!
//! 通信方向：MT5 内的 PABridge EA 以 HTTP 轮询方式（WebRequest）访问本服务的
//! `/bridge/sync` 端点：上报账户/持仓/挂单/合约规格/K 线，并取回待执行指令。
//! 本模块是 `/bridge/sync` 的服务端实现，对上层提供与旧 OKX 客户端同构的
//! 异步接口（get_candles / get_positions / 下单 / 撤单 / 改止盈止损等）。

use super::protocol::{parse_sync, serialize_commands, ParsedSync};
use super::types::{Command, CommandAction};
use super::types::*;
use crate::data::base::KlineBar;
use anyhow::{anyhow, bail, Result};
use parking_lot::{Mutex, RwLock};
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::oneshot;

/// 默认 Magic Number（需与 EA 输入参数 MagicNumber 一致）。
pub const DEFAULT_MAGIC: i64 = 20260907;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 把 MT5 时间周期文本（M15/H1/D1...）换算为秒。
pub fn mt5_tf_to_seconds(tf: &str) -> u64 {
    let t = tf.trim().to_uppercase();
    if let Some(num) = t.strip_prefix('M') {
        if let Ok(n) = num.parse::<u64>() {
            return n * 60;
        }
    }
    if let Some(num) = t.strip_prefix('H') {
        if let Ok(n) = num.parse::<u64>() {
            return n * 3600;
        }
    }
    match t.as_str() {
        "D1" => 86400,
        "W1" => 7 * 86400,
        "MN1" => 30 * 86400,
        _ => 0,
    }
}

#[derive(Debug, Clone, Default)]
pub struct BridgeState {
    pub last_sync_ms: i64,
    pub terminal: TerminalInfo,
    pub account: BridgeAccount,
    pub symbols: HashMap<String, SymbolSpec>,
    pub positions: Vec<BridgePosition>,
    pub orders: Vec<BridgeOrder>,
    /// 系列键 "SYM:TF" -> K 线（新->旧排列，与代理内部约定一致）
    pub bars: HashMap<String, Vec<KlineBar>>,
}

/// 单次同步允许下发的最大指令数（防止响应体过大）。
const MAX_COMMANDS_PER_SYNC: usize = 64;

#[derive(Debug, Clone)]
pub struct MT5Bridge {
    token: Option<String>,
    magic: i64,
    command_timeout: Duration,
    stale: Duration,
    state: Arc<RwLock<BridgeState>>,
    queue: Arc<Mutex<VecDeque<Command>>>,
    waiters: Arc<Mutex<HashMap<String, oneshot::Sender<CmdResult>>>>,
    auto_series: Arc<Mutex<HashSet<String>>>,
    wanted_specs: Arc<Mutex<HashSet<String>>>,
}

impl MT5Bridge {
    pub fn new(
        token: &str,
        magic: i64,
        command_timeout_seconds: u64,
        stale_seconds: u64,
    ) -> Self {
        Self {
            token: {
                let t = token.trim().to_string();
                if t.is_empty() { None } else { Some(t) }
            },
            magic: if magic != 0 { magic } else { DEFAULT_MAGIC },
            command_timeout: Duration::from_secs(command_timeout_seconds.max(5)),
            stale: Duration::from_secs(stale_seconds.max(5)),
            state: Arc::new(RwLock::new(BridgeState::default())),
            queue: Arc::new(Mutex::new(VecDeque::new())),
            waiters: Arc::new(Mutex::new(HashMap::new())),
            auto_series: Arc::new(Mutex::new(HashSet::new())),
            wanted_specs: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    pub fn magic(&self) -> i64 {
        self.magic
    }

    pub fn last_sync_ms(&self) -> i64 {
        self.state.read().last_sync_ms
    }

    pub fn is_connected(&self) -> bool {
        let s = self.state.read();
        s.last_sync_ms > 0
            && (now_ms() - s.last_sync_ms) <= self.stale.as_millis() as i64
    }

    /// EA 上报的最近一次同步摘要（用于状态面板）。
    pub fn terminal_summary(&self) -> serde_json::Value {
        let s = self.state.read();
        serde_json::json!({
            "connected": self.is_connected(),
            "last_sync_ms": s.last_sync_ms,
            "terminal_name": s.terminal.name,
            "company": s.terminal.company,
            "terminal_connected": s.terminal.connected,
            "trade_allowed": s.terminal.trade_allowed,
            "server_time_unix": s.terminal.server_time_unix,
            "account": account_to_json(&s.account),
            "series_count": s.bars.len(),
            "symbol_count": s.symbols.len(),
        })
    }

    /// 服务器当前时间（毫秒），用于 K 线闭合判断，避免 MT5 服务器时区偏差。
    pub fn server_now_ms(&self) -> i64 {
        let s = self.state.read();
        if s.terminal.server_time_unix > 0 {
            let elapsed = (now_ms() - s.terminal.reported_at_ms).max(0);
            (s.terminal.server_time_unix * 1000) + elapsed
        } else {
            now_ms()
        }
    }

    fn ensure_series(&self, symbol: &str, tf: &str, limit: usize) {
        let key = series_key(symbol, tf);
        let mut auto = self.auto_series.lock();
        if auto.contains(&key) {
            return;
        }
        auto.insert(key.clone());
        drop(auto);
        let bars = limit.max(400).min(720);
        self.queue
            .lock()
            .push_back(Command { id: new_cmd_id("sub"), action: CommandAction::Subscribe { series: key, bars } });
    }

    fn ensure_spec(&self, symbol: &str) {
        let upper = symbol.trim().to_uppercase();
        let mut wanted = self.wanted_specs.lock();
        if wanted.contains(&upper) {
            return;
        }
        wanted.insert(upper.clone());
        drop(wanted);
        self.queue
            .lock()
            .push_back(Command { id: new_cmd_id("spec"), action: CommandAction::Spec { symbol: upper } });
    }

    /// 下发指令并等待 EA 回传结果。
    pub async fn exec(&self, action: CommandAction) -> Result<CmdResult> {
        if !self.is_connected() {
            bail!(
                "MT5 桥接未连接：请确认 MT5 终端已启动、PABridge EA 已挂载到图表，\
                 且已在 MT5 选项中允许对 http://127.0.0.1:8066 的 WebRequest 访问"
            );
        }
        let id = new_cmd_id("cmd");
        let (tx, rx) = oneshot::channel();
        self.waiters.lock().insert(id.clone(), tx);
        let summary = describe_action(&action);
        self.queue
            .lock()
            .push_back(Command { id: id.clone(), action });

        tracing::info!("[桥接] 指令已入队待 EA 领取: id={} | {}", id, summary);

        match tokio::time::timeout(self.command_timeout, rx).await {
            Ok(Ok(res)) => {
                if res.ok {
                    tracing::info!(
                        "[桥接] EA 执行成功: id={} | {} | 回执={}",
                        id,
                        res.msg,
                        res.data
                    );
                    Ok(res)
                } else {
                    tracing::warn!("[桥接] EA 执行失败: id={} | {} | {}", id, summary, res.msg);
                    Err(anyhow!("MT5 执行失败: {}", res.msg))
                }
            }
            _ => {
                self.waiters.lock().remove(&id);
                if !self.is_connected() {
                    bail!("MT5 桥接连接中断，指令 {} 未得到确认", id)
                } else {
                    bail!(
                        "等待 MT5 执行结果超时（{} 秒）。请检查 EA 是否正在运行、\
                         日志中是否有错误（如命令执行失败或轮询阻塞）",
                        self.command_timeout.as_secs()
                    )
                }
            }
        }
    }

    /// `/bridge/sync` 服务端入口：解析上报、更新缓存、分发结果、返回待执行指令。
    pub fn handle_sync(&self, auth_header: Option<&str>, body: &str) -> Result<String> {
        let parsed: ParsedSync = parse_sync(body)?;

        if let Some(expected) = &self.token {
            let provided = parsed
                .auth
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    auth_header
                        .and_then(|h| h.strip_prefix("Bearer "))
                        .map(str::trim)
                });
            match provided {
                Some(p) if p == expected.trim() => {}
                _ => bail!("桥接鉴权失败：BridgeToken 不匹配"),
            }
        }

        self.apply_state(&parsed);

        // 分发指令结果给等待者
        {
            let mut waiters = self.waiters.lock();
            for res in &parsed.results {
                if let Some(tx) = waiters.remove(&res.id) {
                    let _ = tx.send(res.clone());
                }
            }
        }

        // 取出待执行指令（上限保护响应体大小）
        let mut commands: Vec<Command> = Vec::new();
        {
            let mut q = self.queue.lock();
            while commands.len() < MAX_COMMANDS_PER_SYNC {
                match q.pop_front() {
                    Some(c) => commands.push(c),
                    None => break,
                }
            }
        }
        if !commands.is_empty() {
            let summaries: Vec<String> = commands
                .iter()
                .map(|c| format!("{}({})", c.id, describe_action(&c.action)))
                .collect();
            tracing::info!("[桥接] EA 本次同步领取 {} 条指令: {}", commands.len(), summaries.join(" ; "));
        }
        Ok(serialize_commands(&commands))
    }

    fn apply_state(&self, parsed: &ParsedSync) {
        let ts = now_ms();
        let srvtime = parsed.terminal.server_time_unix;
        let mut state = self.state.write();

        let mut terminal = parsed.terminal.clone();
        terminal.reported_at_ms = ts;
        state.terminal = terminal;
        state.account = parsed.account.clone();
        state.last_sync_ms = ts;

        for spec in &parsed.symbols {
            state.symbols.insert(spec.symbol.clone(), spec.clone());
        }

        state.positions = parsed.positions.clone();
        state.orders = parsed.orders.clone();

        for (key, raw_bars) in &parsed.bars {
            let tf = key.rsplit(':').next().unwrap_or("");
            let tf_seconds = mt5_tf_to_seconds(tf) as i64;
            let server_now = srvtime;
            let mut bars: Vec<KlineBar> = raw_bars
                .iter()
                .enumerate()
                .map(|(_i, b)| {
                    let closed = if tf_seconds > 0 && server_now > 0 {
                        b.time + tf_seconds <= server_now
                    } else {
                        true
                    };
                    KlineBar {
                        seq: 0,
                        ts_open: b.time * 1000,
                        open: b.open,
                        high: b.high,
                        low: b.low,
                        close: b.close,
                        volume: b.volume,
                        amount: 0.0,
                        pct_chg: None,
                        closed,
                    }
                })
                .collect();
            // EA 上报为时间升序；代理内部约定 bars[0] 为最新
            bars.reverse();
            for (i, bar) in bars.iter_mut().enumerate() {
                bar.seq = i + 1;
                if !bar.closed {
                    bar.seq = 0;
                }
            }
            state.bars.insert(key.clone(), bars);
        }
    }

    /// 在 K 线缓存中查找系列（品种名统一以 EA 上报为准）。
    /// 先按 series_key 精确查找，再对缓存键做大小写不敏感兜底
    /// （EA 上报键可能是终端规范名，如 XAUUSDm:M15）。
    fn bars_lookup<'a>(state: &'a BridgeState, symbol: &str, tf: &str) -> Option<&'a Vec<KlineBar>> {
        let key = series_key(symbol, tf);
        if let Some(v) = state.bars.get(&key) {
            return Some(v);
        }
        state
            .bars
            .iter()
            .find(|(k, _)| match k.rsplit_once(':') {
                Some((sym, t)) => sym.eq_ignore_ascii_case(symbol.trim()) && t.eq_ignore_ascii_case(tf),
                None => false,
            })
            .map(|(_, v)| v)
    }

    /// 把请求的品种名解析为 EA 实际上报的终端规范名。
    ///
    /// 品种名统一以 EA 上报为准，匹配顺序：
    /// 1) 精确匹配；2) 大小写不敏感；3) 前缀/后缀兼容（如配置名
    /// XAUUSD 解析为经纪商规范名 XAUUSDm，取附加后缀最短者）。
    /// 候选名来自 EA 上报的合约规格、K 线系列键、持仓与挂单。
    pub fn resolve_symbol(&self, requested: &str) -> Option<String> {
        let req = requested.trim();
        if req.is_empty() {
            return None;
        }

        let state = self.state.read();
        if state.symbols.is_empty() && state.bars.is_empty() && state.positions.is_empty() {
            return None;
        }
        let mut names: Vec<String> = Vec::new();
        names.extend(state.symbols.keys().cloned());
        for key in state.bars.keys() {
            if let Some((sym, _tf)) = key.rsplit_once(':') {
                if !sym.is_empty() {
                    names.push(sym.to_string());
                }
            }
        }
        for p in &state.positions {
            names.push(p.symbol.clone());
        }
        for o in &state.orders {
            names.push(o.symbol.clone());
        }
        drop(state);
        names.sort();
        names.dedup();

        if names.iter().any(|n| n == req) {
            return Some(req.to_string());
        }
        if let Some(n) = names.iter().find(|n| n.eq_ignore_ascii_case(req)) {
            return Some(n.clone());
        }
        let req_up = req.to_uppercase();
        let mut best: Option<&String> = None;
        for n in &names {
            let n_up = n.to_uppercase();
            let compatible = n_up.starts_with(&req_up) || req_up.starts_with(&n_up);
            if compatible {
                match best {
                    // 取附加后缀最短的候选（如 XAUUSDm 优于 XAUUSDmicro）
                    Some(b) if b.len() <= n.len() => {}
                    _ => best = Some(n),
                }
            }
        }
        best.cloned()
    }

    pub async fn get_candles(&self, symbol: &str, timeframe: &str, limit: usize) -> Result<Vec<KlineBar>> {
        // 系列键与 EA 上报保持一致：品种 + MT5 周期文本（如 XAUUSD:M15）
        let mt5_tf = timeframe_to_mt5(timeframe)
            .ok_or_else(|| anyhow!("不支持的周期: {}（MT5 支持 1m/5m/15m/30m/1h/4h/1d/1w 等）", timeframe))?;
        let requested = symbol.trim();
        let want = limit.clamp(10, 300);
        let deadline = Instant::now() + Duration::from_secs(10);

        loop {
            // 品种名统一以 EA 上报为准：优先解析为终端规范名再订阅/查缓存
            let primary = self.resolve_symbol(requested).unwrap_or_else(|| requested.to_string());
            self.ensure_series(&primary, mt5_tf, want);

            {
                let state = self.state.read();
                if let Some(bars) = Self::bars_lookup(&state, &primary, mt5_tf)
                    .or_else(|| Self::bars_lookup(&state, requested, mt5_tf))
                {
                    if !bars.is_empty() {
                        let mut out: Vec<KlineBar> = bars.iter().take(want).cloned().collect();
                        // seq 约定：1 = 最新已收盘 K 线，未收盘 K 线 seq = 0
                        let mut closed_idx = 0;
                        for b in out.iter_mut() {
                            if b.closed {
                                closed_idx += 1;
                                b.seq = closed_idx;
                            } else {
                                b.seq = 0;
                            }
                        }
                        return Ok(out);
                    }
                }
            }
            if !self.is_connected() {
                bail!(
                    "MT5 桥接未连接，无法获取 {}:{} K 线。请启动 MT5 并挂载 PABridge EA",
                    primary,
                    mt5_tf
                );
            }
            if Instant::now() >= deadline {
                bail!(
                    "等待 MT5 推送 {}:{} K 线超时：请确认 EA 输入参数或界面请求的品种在当前账户中可用",
                    primary,
                    mt5_tf
                );
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    pub async fn get_symbol_spec(&self, symbol: &str) -> Result<SymbolSpec> {
        let upper = symbol.trim().to_uppercase();
        self.ensure_spec(&upper);

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            {
                let state = self.state.read();
                // 精确 -> 大写 -> 大小写不敏感兜底（EA 端上报的是终端规范名，如 XAUUSDm）
                if let Some(spec) = state
                    .symbols
                    .get(symbol.trim())
                    .or_else(|| state.symbols.get(&upper))
                    .or_else(|| {
                        state
                            .symbols
                            .values()
                            .find(|s| s.symbol.eq_ignore_ascii_case(symbol.trim()))
                    })
                {
                    return Ok(spec.clone());
                }
            }
            // 前缀/后缀解析兜底：品种名统一以 EA 上报为准（如 XAUUSD -> XAUUSDm）
            if let Some(resolved) = self.resolve_symbol(symbol) {
                if !resolved.eq_ignore_ascii_case(symbol.trim()) {
                    self.ensure_spec(&resolved);
                    let state = self.state.read();
                    if let Some(spec) = state
                        .symbols
                        .get(&resolved)
                        .or_else(|| {
                            state
                                .symbols
                                .values()
                                .find(|s| s.symbol.eq_ignore_ascii_case(&resolved))
                        })
                    {
                        return Ok(spec.clone());
                    }
                }
            }
            if !self.is_connected() {
                bail!("MT5 桥接未连接，无法查询品种规格 {}", upper);
            }
            if Instant::now() >= deadline {
                bail!(
                    "等待品种 {} 规格上报超时：该品种可能不存在于当前 MT5 终端（注意大小写与经纪商后缀，如 XAUUSDm）",
                    symbol.trim()
                );
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    pub fn get_account(&self) -> BridgeAccount {
        self.state.read().account.clone()
    }

    pub fn get_positions(&self, symbol: Option<&str>) -> Vec<BridgePosition> {
        // 品种名统一以 EA 上报为准：除大小写不敏感外，兼容解析名（如 XAUUSD -> XAUUSDm）
        let resolved = symbol.and_then(|s| self.resolve_symbol(s));
        let state = self.state.read();
        match symbol {
            Some(s) => state
                .positions
                .iter()
                .filter(|p| {
                    p.symbol.eq_ignore_ascii_case(s.trim())
                        || resolved
                            .as_deref()
                            .map(|r| p.symbol.eq_ignore_ascii_case(r))
                            .unwrap_or(false)
                })
                .cloned()
                .collect(),
            None => state.positions.clone(),
        }
    }

    pub fn get_pending_orders(&self, symbol: Option<&str>) -> Vec<BridgeOrder> {
        let resolved = symbol.and_then(|s| self.resolve_symbol(s));
        let state = self.state.read();
        match symbol {
            Some(s) => state
                .orders
                .iter()
                .filter(|o| {
                    o.symbol.eq_ignore_ascii_case(s.trim())
                        || resolved
                            .as_deref()
                            .map(|r| o.symbol.eq_ignore_ascii_case(r))
                            .unwrap_or(false)
                })
                .cloned()
                .collect(),
            None => state.orders.clone(),
        }
    }

    pub fn list_symbols(&self) -> Vec<SymbolSpec> {
        let state = self.state.read();
        let mut specs: Vec<SymbolSpec> = state.symbols.values().cloned().collect();
        specs.sort_by(|a, b| a.symbol.cmp(&b.symbol));
        specs
    }

    pub async fn place_market(&self, symbol: &str, side: &str, volume: f64, sl: f64, tp: f64, comment: &str) -> Result<CmdResult> {
        self.exec(CommandAction::Market {
            symbol: symbol.trim().to_string(),
            side: side.to_string(),
            volume,
            sl,
            tp,
            comment: comment.to_string(),
            magic: self.magic,
        })
        .await
    }

    pub async fn place_limit(&self, symbol: &str, side: &str, volume: f64, price: f64, sl: f64, tp: f64, comment: &str) -> Result<CmdResult> {
        self.exec(CommandAction::Limit {
            symbol: symbol.trim().to_string(),
            side: side.to_string(),
            volume,
            price,
            sl,
            tp,
            comment: comment.to_string(),
            magic: self.magic,
        })
        .await
    }

    pub async fn place_stop(&self, symbol: &str, side: &str, volume: f64, price: f64, sl: f64, tp: f64, comment: &str) -> Result<CmdResult> {
        self.exec(CommandAction::Stop {
            symbol: symbol.trim().to_string(),
            side: side.to_string(),
            volume,
            price,
            sl,
            tp,
            comment: comment.to_string(),
            magic: self.magic,
        })
        .await
    }

    pub async fn cancel_order(&self, ticket: i64) -> Result<CmdResult> {
        self.exec(CommandAction::Cancel { ticket }).await
    }

    pub async fn modify_position_sltp(&self, ticket: i64, sl: Option<f64>, tp: Option<f64>) -> Result<CmdResult> {
        self.exec(CommandAction::ModifySltp { ticket, sl, tp }).await
    }

    pub async fn close_position(&self, ticket: i64) -> Result<CmdResult> {
        self.exec(CommandAction::Close { ticket }).await
    }
}

use std::sync::Arc;

fn new_cmd_id(prefix: &str) -> String {
    use uuid::Uuid;
    format!("{}-{}", prefix, Uuid::new_v4().simple())
}

/// 指令的人类可读摘要（用于详细日志）。
fn describe_action(action: &CommandAction) -> String {
    match action {
        CommandAction::Market { symbol, side, volume, sl, tp, .. } => {
            format!("市价单 {} {} 手数={} SL={} TP={}", symbol, side, volume, sl, tp)
        }
        CommandAction::Limit { symbol, side, volume, price, sl, tp, .. } => {
            format!("限价单 {} {} 手数={} 价格={} SL={} TP={}", symbol, side, volume, price, sl, tp)
        }
        CommandAction::Stop { symbol, side, volume, price, sl, tp, .. } => {
            format!("突破单 {} {} 手数={} 触发价={} SL={} TP={}", symbol, side, volume, price, sl, tp)
        }
        CommandAction::Cancel { ticket } => format!("撤销挂单 #{}", ticket),
        CommandAction::ModifySltp { ticket, sl, tp } => {
            let f = |v: &Option<f64>| v.map(|x| x.to_string()).unwrap_or_else(|| "保持".to_string());
            format!("修改持仓 SL/TP #{} SL={} TP={}", ticket, f(sl), f(tp))
        }
        CommandAction::Close { ticket } => format!("市价平仓 #{}", ticket),
        CommandAction::Subscribe { series, bars } => format!("订阅 K 线系列 {}（{} 根）", series, bars),
        CommandAction::Unsubscribe { series } => format!("退订 K 线系列 {}", series),
        CommandAction::Spec { symbol } => format!("请求品种规格 {}", symbol),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个已收到 EA 同步上报的桥接：
    /// 规格与持仓/挂单上报为经纪商规范名 XAUUSDm，K 线系列键为 XAUUSDM:M5，
    /// 而请求名是配置里的 XAUUSD —— 复现 Exness 类后缀品种的解析场景。
    fn bridge_with_ea_state() -> MT5Bridge {
        let bridge = MT5Bridge::new("", 0, 20, 15);
        let body = "\
T2A 1\n\
TERM name=T|company=C|connected=1|trade_allowed=1|srvtime=1700000000\n\
ACCT login=1|name=d|currency=USD|balance=1000|equity=1000|margin=0|freemargin=1000|marginlevel=0|profit=0|leverage=100|trademode=0|marginmode=2|server=S\n\
SYM XAUUSDm|desc=Gold|point=0.01|digits=2|tick=0.01|vmin=0.01|vstep=0.01|vmax=100|contract=100|margininit=0|bid=2400|ask=2400.5|last=0|trademode=4\n\
POS 111|XAUUSDm|buy|0.1|2400|2401|0|0|10|0|0|1700000000|20260907|pa\n\
ORD 222|XAUUSDm|sell_limit|0.2|2450|0|0|1700000000|20260907|pa\n\
BAR XAUUSDM:M5\n\
BART 1699999700,2400,2401,2399,2400.5,10\n\
BART 1700000000,2400.5,2402,2400,2401,10\n\
END\n";
        bridge.handle_sync(None, body).expect("sync 应解析成功");
        bridge
    }

    #[test]
    fn test_resolve_symbol_prefix_suffix() {
        let bridge = bridge_with_ea_state();
        // 配置名 XAUUSD 解析为 EA 上报的规范名
        assert_eq!(bridge.resolve_symbol("XAUUSD").as_deref(), Some("XAUUSDM"));
        // 大小写不敏感命中规格上报名
        assert_eq!(bridge.resolve_symbol("XAUUSDm").as_deref(), Some("XAUUSDm"));
        // 完全未知的品种不解析
        assert_eq!(bridge.resolve_symbol("EURUSD"), None);
        assert_eq!(bridge.resolve_symbol(""), None);
    }

    #[tokio::test]
    async fn test_get_candles_uses_resolved_name() {
        let bridge = bridge_with_ea_state();
        // 用配置名 XAUUSD 查 M5：应通过解析名 XAUUSDM:M5 命中 EA 上报的缓存
        let bars = bridge.get_candles("XAUUSD", "5m", 10).await.expect("应取到 K 线");
        assert_eq!(bars.len(), 2);
        assert_eq!(bars[0].close, 2401.0); // 最新在前
    }

    #[tokio::test]
    async fn test_get_symbol_spec_uses_resolved_name() {
        let bridge = bridge_with_ea_state();
        let spec = bridge.get_symbol_spec("XAUUSD").await.expect("应取到规格");
        assert_eq!(spec.symbol, "XAUUSDm");
    }

    #[test]
    fn test_positions_and_orders_match_resolved_symbol() {
        let bridge = bridge_with_ea_state();
        // 用配置名过滤持仓/挂单：应通过解析名匹配到 XAUUSDm 的记录
        assert_eq!(bridge.get_positions(Some("XAUUSD")).len(), 1);
        assert_eq!(bridge.get_pending_orders(Some("XAUUSD")).len(), 1);
        assert_eq!(bridge.get_positions(Some("EURUSD")).len(), 0);
    }
}
