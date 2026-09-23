use serde::{Deserialize, Serialize};
use serde_json::Value;

/// MT5 终端信息（由 EA 随每次同步上报）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalInfo {
    pub name: String,
    pub company: String,
    pub connected: bool,
    pub trade_allowed: bool,
    /// EA 上报的服务器时间（TimeCurrent，Unix 秒）。
    pub server_time_unix: i64,
    /// 上报时间（本机时钟，Unix 毫秒）。
    pub reported_at_ms: i64,
}

impl Default for TerminalInfo {
    fn default() -> Self {
        Self {
            name: String::new(),
            company: String::new(),
            connected: false,
            trade_allowed: false,
            server_time_unix: 0,
            reported_at_ms: 0,
        }
    }
}

/// MT5 账户摘要（EA 上报）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeAccount {
    pub login: i64,
    pub name: String,
    pub currency: String,
    pub balance: f64,
    pub equity: f64,
    pub margin_used: f64,
    pub free_margin: f64,
    pub margin_level: f64,
    pub profit: f64,
    pub leverage: i64,
    /// demo / contest / real / unknown
    pub trade_mode: String,
    /// netting / hedging / exchange / unknown
    pub margin_mode: String,
    pub server: String,
}

impl Default for BridgeAccount {
    fn default() -> Self {
        Self {
            login: 0,
            name: String::new(),
            currency: "USD".to_string(),
            balance: 0.0,
            equity: 0.0,
            margin_used: 0.0,
            free_margin: 0.0,
            margin_level: 0.0,
            profit: 0.0,
            leverage: 100,
            trade_mode: "unknown".to_string(),
            margin_mode: "unknown".to_string(),
            server: String::new(),
        }
    }
}

/// MT5 品种规格（EA 上报，来自 SymbolInfo*）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolSpec {
    pub symbol: String,
    pub description: String,
    pub point: f64,
    pub digits: i64,
    pub tick_size: f64,
    pub volume_min: f64,
    pub volume_step: f64,
    pub volume_max: f64,
    /// 一手对应的标的合约数量（如 XAUUSD 100）。
    pub contract_size: f64,
    /// 一手初始保证金（部分经纪商提供；0 表示未知，按杠杆估算）。
    pub margin_initial: f64,
    pub bid: f64,
    pub ask: f64,
    pub last: f64,
    /// full / closeonly / disabled
    pub trade_mode: String,
}

impl Default for SymbolSpec {
    fn default() -> Self {
        Self {
            symbol: String::new(),
            description: String::new(),
            point: 0.0,
            digits: 5,
            tick_size: 0.0,
            volume_min: 0.01,
            volume_step: 0.01,
            volume_max: 100.0,
            contract_size: 1.0,
            margin_initial: 0.0,
            bid: 0.0,
            ask: 0.0,
            last: 0.0,
            trade_mode: "full".to_string(),
        }
    }
}

/// MT5 持仓（EA 上报）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgePosition {
    pub ticket: i64,
    pub symbol: String,
    /// long / short
    pub side: String,
    pub volume: f64,
    pub price_open: f64,
    pub price_current: f64,
    pub sl: f64,
    pub tp: f64,
    pub profit: f64,
    pub swap: f64,
    pub commission: f64,
    /// 开仓时间（Unix 秒，服务器时间）
    pub time: i64,
    pub magic: i64,
    pub comment: String,
}

/// MT5 挂单（EA 上报，含 limit/stop/stoplimit）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeOrder {
    pub ticket: i64,
    pub symbol: String,
    /// buy_limit / sell_limit / buy_stop / sell_stop / buy_stoplimit / sell_stoplimit
    pub kind: String,
    pub volume: f64,
    pub price: f64,
    pub sl: f64,
    pub tp: f64,
    pub time: i64,
    pub magic: i64,
    pub comment: String,
}

/// 桥接指令执行结果（EA 回传）。
#[derive(Debug, Clone)]
pub struct CmdResult {
    pub id: String,
    pub ok: bool,
    pub msg: String,
    pub data: Value,
}

/// 代理可执行的原子操作枚举（Rust -> EA）。
#[derive(Debug, Clone)]
pub enum CommandAction {
    Market {
        symbol: String,
        side: String,
        volume: f64,
        sl: f64,
        tp: f64,
        comment: String,
        magic: i64,
    },
    Limit {
        symbol: String,
        side: String,
        volume: f64,
        price: f64,
        sl: f64,
        tp: f64,
        comment: String,
        magic: i64,
    },
    Stop {
        symbol: String,
        side: String,
        volume: f64,
        price: f64,
        sl: f64,
        tp: f64,
        comment: String,
        magic: i64,
    },
    Cancel {
        ticket: i64,
    },
    /// 修改持仓的止损/止盈；None 表示保持不变。
    ModifySltp {
        ticket: i64,
        sl: Option<f64>,
        tp: Option<f64>,
    },
    Close {
        ticket: i64,
    },
    Subscribe {
        series: String,
        bars: usize,
    },
    Unsubscribe {
        series: String,
    },
    Spec {
        symbol: String,
    },
}

/// 带唯一 ID 的桥接指令。
#[derive(Debug, Clone)]
pub struct Command {
    pub id: String,
    pub action: CommandAction,
}

/// 把代理使用的周期字符串映射为 MT5 时间周期（EA 端 ENUM_TIMEFRAMES 文本）。
pub fn timeframe_to_mt5(timeframe: &str) -> Option<&'static str> {
    match timeframe.trim().to_lowercase().as_str() {
        "1m" => Some("M1"),
        "2m" => Some("M2"),
        "3m" => Some("M3"),
        "4m" => Some("M4"),
        "5m" => Some("M5"),
        "6m" => Some("M6"),
        "10m" => Some("M10"),
        "12m" => Some("M12"),
        "15m" => Some("M15"),
        "20m" => Some("M20"),
        "30m" => Some("M30"),
        "1h" => Some("H1"),
        "2h" => Some("H2"),
        "3h" => Some("H3"),
        "4h" => Some("H4"),
        "6h" => Some("H6"),
        "8h" => Some("H8"),
        "12h" => Some("H12"),
        "1d" | "1D" => Some("D1"),
        "1w" | "1W" => Some("W1"),
        "1M" | "1mo" => Some("MN1"),
        _ => None,
    }
}

/// 桥接系列键，如 "XAUUSD:M15"。
pub fn series_key(symbol: &str, timeframe: &str) -> String {
    format!("{}:{}", symbol.trim().to_uppercase(), timeframe.trim())
}

pub fn position_to_json(p: &BridgePosition) -> Value {
    serde_json::json!({
        "ticket": p.ticket,
        "symbol": p.symbol,
        "side": p.side,
        "volume": p.volume,
        "price_open": p.price_open,
        "price_current": p.price_current,
        "sl": p.sl,
        "tp": p.tp,
        "profit": p.profit,
        "swap": p.swap,
        "commission": p.commission,
        "time": p.time,
        "magic": p.magic,
        "comment": p.comment,
    })
}

pub fn order_to_json(o: &BridgeOrder) -> Value {
    serde_json::json!({
        "ticket": o.ticket,
        "symbol": o.symbol,
        "kind": o.kind,
        "volume": o.volume,
        "price": o.price,
        "sl": o.sl,
        "tp": o.tp,
        "time": o.time,
        "magic": o.magic,
        "comment": o.comment,
    })
}

pub fn account_to_json(a: &BridgeAccount) -> Value {
    serde_json::json!({
        "login": a.login,
        "name": a.name,
        "currency": a.currency,
        "balance": a.balance,
        "equity": a.equity,
        "margin_used": a.margin_used,
        "free_margin": a.free_margin,
        "margin_level": a.margin_level,
        "profit": a.profit,
        "leverage": a.leverage,
        "trade_mode": a.trade_mode,
        "margin_mode": a.margin_mode,
        "server": a.server,
    })
}

pub fn spec_to_json(s: &SymbolSpec) -> Value {
    serde_json::json!({
        "symbol": s.symbol,
        "description": s.description,
        "point": s.point,
        "digits": s.digits,
        "tick_size": s.tick_size,
        "volume_min": s.volume_min,
        "volume_step": s.volume_step,
        "volume_max": s.volume_max,
        "contract_size": s.contract_size,
        "margin_initial": s.margin_initial,
        "bid": s.bid,
        "ask": s.ask,
        "last": s.last,
        "trade_mode": s.trade_mode,
    })
}
