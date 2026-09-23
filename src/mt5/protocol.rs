//! MT5 桥接分行文本协议 v1。
//!
//! EA -> Agent（POST /bridge/sync 请求体，UTF-8 纯文本）：
//! ```text
//! T2A 1
//! AUTH <token>                       (可选，配置了 BridgeToken 时必须匹配)
//! TERM name=..|company=..|connected=1|trade_allowed=1|srvtime=1700000000
//! ACCT login=..|name=..|currency=USD|balance=..|equity=..|margin=..|freemargin=..|marginlevel=..|profit=..|leverage=100|trademode=0|marginmode=2|server=..
//! SYM XAUUSD|desc=..|point=0.01|digits=2|tick=0.01|vmin=0.01|vstep=0.01|vmax=100|contract=100|margininit=0|bid=..|ask=..|last=..|trademode=4
//! POS ticket|symbol|side|volume|open|current|sl|tp|profit|swap|commission|time|magic|comment
//! ORD ticket|symbol|kind|volume|price|sl|tp|time|magic|comment
//! BAR XAUUSD:M15
//! BART time,open,high,low,close,volume     (时间升序，最新一根在最后)
//! RES id=..|ok=1|msg=..|data=..
//! END
//! ```
//!
//! Agent -> EA（响应体）：
//! ```text
//! A2T 1
//! CMD id=..|action=MARKET|symbol=XAUUSD|side=buy|volume=0.10|sl=..|tp=..|comment=pa..|magic=20260907
//! CMD id=..|action=LIMIT|symbol=..|side=..|volume=..|price=..|sl=..|tp=..|comment=..|magic=..
//! CMD id=..|action=STOP|...
//! CMD id=..|action=CANCEL|ticket=..
//! CMD id=..|action=MODIFY|ticket=..|sl=..|tp=..     (留空表示保持不变)
//! CMD id=..|action=CLOSE|ticket=..
//! CMD id=..|action=SUBSCRIBE|series=XAUUSD:M15|bars=400
//! CMD id=..|action=UNSUBSCRIBE|series=XAUUSD:M15
//! CMD id=..|action=SPEC|symbol=XAUUSD
//! END
//! ```

use super::types::*;
use anyhow::{anyhow, Result};
use serde_json::Value;

/// EA 上报的一根原始 K 线（时间升序）。
#[derive(Debug, Clone)]
pub struct RawBar {
    pub time: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

/// 一次同步请求解析结果。
#[derive(Debug, Clone)]
pub struct ParsedSync {
    pub auth: Option<String>,
    pub terminal: TerminalInfo,
    pub account: BridgeAccount,
    pub symbols: Vec<SymbolSpec>,
    pub positions: Vec<BridgePosition>,
    pub orders: Vec<BridgeOrder>,
    /// (系列键 "SYM:TF"，升序原始 K 线)
    pub bars: Vec<(String, Vec<RawBar>)>,
    pub results: Vec<CmdResult>,
}

/// 字段值清洗：避免分隔符破坏行协议结构。
pub fn sanitize_field(s: &str) -> String {
    s.replace('|', "／")
        .replace('\n', " ")
        .replace('\r', " ")
        .replace('=', "＝")
}

fn parse_kv_map(s: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for part in s.split('|') {
        if let Some(eq) = part.find('=') {
            let k = part[..eq].trim().to_string();
            let v = part[eq + 1..].trim().to_string();
            if !k.is_empty() {
                map.insert(k, v);
            }
        }
    }
    map
}

fn kv_f64(map: &std::collections::HashMap<String, String>, key: &str, default: f64) -> f64 {
    map.get(key).and_then(|v| v.parse::<f64>().ok()).unwrap_or(default)
}

fn kv_i64(map: &std::collections::HashMap<String, String>, key: &str, default: i64) -> i64 {
    map.get(key).and_then(|v| v.parse::<i64>().ok()).unwrap_or(default)
}

fn kv_s<'a>(map: &'a std::collections::HashMap<String, String>, key: &str, default: &'a str) -> String {
    map.get(key).cloned().unwrap_or_else(|| default.to_string())
}

fn split_fields(line: &str) -> Vec<&str> {
    line.split('|').collect()
}

fn field<'a>(parts: &[&'a str], idx: usize) -> &'a str {
    parts.get(idx).copied().unwrap_or("")
}

fn field_f64(parts: &[&str], idx: usize) -> f64 {
    field(&parts, idx).parse::<f64>().unwrap_or(0.0)
}

fn field_i64(parts: &[&str], idx: usize) -> i64 {
    field(&parts, idx).parse::<i64>().unwrap_or(0)
}

/// 解析 EA 上报的同步请求体。
pub fn parse_sync(body: &str) -> Result<ParsedSync> {
    let mut lines = body.lines().map(|l| l.trim()).filter(|l| !l.is_empty());

    let header = lines.next().ok_or_else(|| anyhow!("空请求体"))?;
    let mut header_parts = header.split_whitespace();
    let magic_word = header_parts.next().unwrap_or("");
    let version = header_parts.next().unwrap_or("");
    if magic_word != "T2A" {
        return Err(anyhow!("协议头不匹配: {}", header));
    }
    if version != "1" {
        return Err(anyhow!("不支持的协议版本: {}", version));
    }

    let mut out = ParsedSync {
        auth: None,
        terminal: TerminalInfo::default(),
        account: BridgeAccount::default(),
        symbols: Vec::new(),
        positions: Vec::new(),
        orders: Vec::new(),
        bars: Vec::new(),
        results: Vec::new(),
    };

    let mut current_series: Option<String> = None;

    for line in lines {
        if line == "END" {
            break;
        }
        let (tag, rest) = match line.find(' ') {
            Some(i) => (&line[..i], &line[i + 1..]),
            None => (line, ""),
        };

        match tag {
            "AUTH" => out.auth = Some(rest.trim().to_string()),
            "TERM" => {
                let m = parse_kv_map(rest);
                out.terminal = TerminalInfo {
                    name: kv_s(&m, "name", ""),
                    company: kv_s(&m, "company", ""),
                    connected: kv_i64(&m, "connected", 0) != 0,
                    trade_allowed: kv_i64(&m, "trade_allowed", 0) != 0,
                    server_time_unix: kv_i64(&m, "srvtime", 0),
                    reported_at_ms: 0,
                };
            }
            "ACCT" => {
                let m = parse_kv_map(rest);
                let trade_mode_code = kv_i64(&m, "trademode", -1);
                let margin_mode_code = kv_i64(&m, "marginmode", -1);
                out.account = BridgeAccount {
                    login: kv_i64(&m, "login", 0),
                    name: kv_s(&m, "name", ""),
                    currency: kv_s(&m, "currency", "USD"),
                    balance: kv_f64(&m, "balance", 0.0),
                    equity: kv_f64(&m, "equity", 0.0),
                    margin_used: kv_f64(&m, "margin", 0.0),
                    free_margin: kv_f64(&m, "freemargin", 0.0),
                    margin_level: kv_f64(&m, "marginlevel", 0.0),
                    profit: kv_f64(&m, "profit", 0.0),
                    leverage: kv_i64(&m, "leverage", 100),
                    trade_mode: match trade_mode_code {
                        0 => "demo".to_string(),
                        1 => "contest".to_string(),
                        2 => "real".to_string(),
                        _ => "unknown".to_string(),
                    },
                    margin_mode: match margin_mode_code {
                        0 => "netting".to_string(),
                        1 => "exchange".to_string(),
                        2 => "hedging".to_string(),
                        _ => "unknown".to_string(),
                    },
                    server: kv_s(&m, "server", ""),
                };
            }
            "SYM" => {
                // 格式: SYM <Symbol>|k=v|k=v|...  第一个字段是品种名，其余全部是 KV
                let symbol = rest.split('|').next().unwrap_or("").trim().to_string();
                if symbol.is_empty() {
                    continue;
                }
                let kv = rest.splitn(2, '|').nth(1).unwrap_or("");
                let m = parse_kv_map(kv);
                out.symbols.push(SymbolSpec {
                    symbol,
                    description: kv_s(&m, "desc", ""),
                    point: kv_f64(&m, "point", 0.0),
                    digits: kv_i64(&m, "digits", 5),
                    tick_size: kv_f64(&m, "tick", 0.0),
                    volume_min: kv_f64(&m, "vmin", 0.01),
                    volume_step: kv_f64(&m, "vstep", 0.01),
                    volume_max: kv_f64(&m, "vmax", 100.0),
                    contract_size: {
                        let c = kv_f64(&m, "contract", 1.0);
                        if c > 0.0 { c } else { 1.0 }
                    },
                    margin_initial: kv_f64(&m, "margininit", 0.0),
                    bid: kv_f64(&m, "bid", 0.0),
                    ask: kv_f64(&m, "ask", 0.0),
                    last: kv_f64(&m, "last", 0.0),
                    trade_mode: match kv_i64(&m, "trademode", 4) {
                        0 => "disabled".to_string(),
                        1 => "closeonly".to_string(),
                        _ => "full".to_string(),
                    },
                });
            }
            "POS" => {
                let parts = split_fields(rest);
                if parts.len() < 2 {
                    continue;
                }
                out.positions.push(BridgePosition {
                    ticket: field_i64(&parts, 0),
                    symbol: field(&parts, 1).trim().to_string(),
                    side: {
                        let s = field(&parts, 2);
                        if s == "1" || s.eq_ignore_ascii_case("short") { "short".to_string() } else { "long".to_string() }
                    },
                    volume: field_f64(&parts, 3),
                    price_open: field_f64(&parts, 4),
                    price_current: field_f64(&parts, 5),
                    sl: field_f64(&parts, 6),
                    tp: field_f64(&parts, 7),
                    profit: field_f64(&parts, 8),
                    swap: field_f64(&parts, 9),
                    commission: field_f64(&parts, 10),
                    time: field_i64(&parts, 11),
                    magic: field_i64(&parts, 12),
                    comment: field(&parts, 13).to_string(),
                });
            }
            "ORD" => {
                let parts = split_fields(rest);
                if parts.len() < 2 {
                    continue;
                }
                out.orders.push(BridgeOrder {
                    ticket: field_i64(&parts, 0),
                    symbol: field(&parts, 1).trim().to_string(),
                    kind: field(&parts, 2).to_string(),
                    volume: field_f64(&parts, 3),
                    price: field_f64(&parts, 4),
                    sl: field_f64(&parts, 5),
                    tp: field_f64(&parts, 6),
                    time: field_i64(&parts, 7),
                    magic: field_i64(&parts, 8),
                    comment: field(&parts, 9).to_string(),
                });
            }
            "BAR" => {
                let key = rest.trim().to_uppercase();
                if !key.is_empty() {
                    current_series = Some(key.clone());
                    out.bars.push((key, Vec::new()));
                }
            }
            "BART" => {
                if let Some(last) = out.bars.last_mut() {
                    let parts: Vec<&str> = rest.split(',').collect();
                    if parts.len() >= 5 {
                        last.1.push(RawBar {
                            time: parts[0].parse::<i64>().unwrap_or(0),
                            open: parts[1].parse::<f64>().unwrap_or(0.0),
                            high: parts[2].parse::<f64>().unwrap_or(0.0),
                            low: parts[3].parse::<f64>().unwrap_or(0.0),
                            close: parts[4].parse::<f64>().unwrap_or(0.0),
                            volume: parts.get(5).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0),
                        });
                    }
                }
            }
            "RES" => {
                // 兼容两种回执格式：
                //   规范键值格式: RES id=..|ok=1|msg=..|data=..
                //   EA 位置格式:  RES id=..|1|执行消息|票据号   (PABridge QueueResult 实际发送，
                //                 San() 已保证 msg/data 中不含 | 与 =)
                // 判别依据：第 2 段是否以 "ok=" 开头。
                let parts: Vec<&str> = rest.split('|').collect();
                let is_kv_format = parts
                    .get(1)
                    .map(|p| p.trim().starts_with("ok="))
                    .unwrap_or(false);
                let (ok, msg, data) = if is_kv_format {
                    let m = parse_kv_map(rest);
                    (
                        kv_i64(&m, "ok", 0) != 0,
                        kv_s(&m, "msg", ""),
                        kv_s(&m, "data", ""),
                    )
                } else {
                    let part = |i: usize| -> String {
                        parts.get(i).map(|p| p.trim().to_string()).unwrap_or_default()
                    };
                    (part(1) == "1", part(2), part(3))
                };
                let id = {
                    let m = parse_kv_map(parts.first().unwrap_or(&"").to_string().as_str());
                    kv_s(&m, "id", "")
                };
                out.results.push(CmdResult {
                    id,
                    ok,
                    msg,
                    data: {
                        if data.is_empty() {
                            Value::Null
                        } else {
                            Value::from(data)
                        }
                    },
                });
            }
            _ => {}
        }
        let _ = &mut current_series;
    }

    Ok(out)
}

fn fmt_f64(v: f64) -> String {
    if v.is_finite() {
        format!("{}", v)
    } else {
        "0".to_string()
    }
}

fn opt_price(v: Option<f64>) -> String {
    match v {
        Some(p) if p.is_finite() && p > 0.0 => fmt_f64(p),
        _ => String::new(),
    }
}

fn cmd_body(parts: Vec<(&str, String)>) -> String {
    parts
        .into_iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect::<Vec<_>>()
        .join("|")
}

/// 把指令序列化为响应体（A2T v1）。
pub fn serialize_commands(commands: &[Command]) -> String {
    let mut out = String::from("A2T 1\n");
    for cmd in commands {
        let body = match &cmd.action {
            CommandAction::Market { symbol, side, volume, sl, tp, comment, magic } => cmd_body(vec![
                ("id", cmd.id.clone()),
                ("action", "MARKET".to_string()),
                ("symbol", symbol.clone()),
                ("side", side.clone()),
                ("volume", fmt_f64(*volume)),
                ("price", String::new()),
                ("sl", fmt_f64(*sl)),
                ("tp", fmt_f64(*tp)),
                ("comment", sanitize_field(comment)),
                ("magic", magic.to_string()),
            ]),
            CommandAction::Limit { symbol, side, volume, price, sl, tp, comment, magic } => cmd_body(vec![
                ("id", cmd.id.clone()),
                ("action", "LIMIT".to_string()),
                ("symbol", symbol.clone()),
                ("side", side.clone()),
                ("volume", fmt_f64(*volume)),
                ("price", fmt_f64(*price)),
                ("sl", fmt_f64(*sl)),
                ("tp", fmt_f64(*tp)),
                ("comment", sanitize_field(comment)),
                ("magic", magic.to_string()),
            ]),
            CommandAction::Stop { symbol, side, volume, price, sl, tp, comment, magic } => cmd_body(vec![
                ("id", cmd.id.clone()),
                ("action", "STOP".to_string()),
                ("symbol", symbol.clone()),
                ("side", side.clone()),
                ("volume", fmt_f64(*volume)),
                ("price", fmt_f64(*price)),
                ("sl", fmt_f64(*sl)),
                ("tp", fmt_f64(*tp)),
                ("comment", sanitize_field(comment)),
                ("magic", magic.to_string()),
            ]),
            CommandAction::Cancel { ticket } => cmd_body(vec![
                ("id", cmd.id.clone()),
                ("action", "CANCEL".to_string()),
                ("ticket", ticket.to_string()),
            ]),
            CommandAction::ModifySltp { ticket, sl, tp } => cmd_body(vec![
                ("id", cmd.id.clone()),
                ("action", "MODIFY".to_string()),
                ("ticket", ticket.to_string()),
                ("sl", opt_price(*sl)),
                ("tp", opt_price(*tp)),
            ]),
            CommandAction::Close { ticket } => cmd_body(vec![
                ("id", cmd.id.clone()),
                ("action", "CLOSE".to_string()),
                ("ticket", ticket.to_string()),
            ]),
            CommandAction::Subscribe { series, bars } => cmd_body(vec![
                ("id", cmd.id.clone()),
                ("action", "SUBSCRIBE".to_string()),
                ("series", series.clone()),
                ("bars", bars.to_string()),
            ]),
            CommandAction::Unsubscribe { series } => cmd_body(vec![
                ("id", cmd.id.clone()),
                ("action", "UNSUBSCRIBE".to_string()),
                ("series", series.clone()),
            ]),
            CommandAction::Spec { symbol } => cmd_body(vec![
                ("id", cmd.id.clone()),
                ("action", "SPEC".to_string()),
                ("symbol", symbol.clone()),
            ]),
        };
        out.push_str("CMD ");
        out.push_str(&body);
        out.push('\n');
    }
    out.push_str("END\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "T2A 1\n\
        AUTH tok123\n\
        TERM name=MetaTrader 5|company=Demo Ltd|connected=1|trade_allowed=1|srvtime=1700000000\n\
        ACCT login=123456|name=Trader|currency=USD|balance=10000.5|equity=10100.5|margin=200|freemargin=9800.5|marginlevel=5050.25|profit=100|leverage=100|trademode=0|marginmode=2|server=Demo-Server\n\
        SYM XAUUSD|desc=Gold|point=0.01|digits=2|tick=0.01|vmin=0.01|vstep=0.01|vmax=50|contract=100|margininit=0|bid=1930.10|ask=1930.30|last=0|trademode=4\n\
        POS 111|XAUUSD|0|0.1|1928.00|1930.00|1925.00|1940.00|20.0|0.5|-2.0|1700000100|20260907|paabc\n\
        ORD 222|XAUUSD|buy_limit|0.2|1920.00|1915.00|1935.00|1700000200|20260907|padef\n\
        BAR XAUUSD:M15\n\
        BART 1699999100,1929.0,1930.0,1928.5,1929.5,120\n\
        BART 1699999200,1929.5,1931.0,1929.0,1930.5,150\n\
        RES id=abc|ok=1|msg=|data=999\n\
        END\n";

    #[test]
    fn test_parse_sync_full() {
        let parsed = parse_sync(SAMPLE).unwrap();
        assert_eq!(parsed.auth.as_deref(), Some("tok123"));
        assert_eq!(parsed.terminal.name, "MetaTrader 5");
        assert!(parsed.terminal.connected);
        assert_eq!(parsed.terminal.server_time_unix, 1700000000);
        assert_eq!(parsed.account.login, 123456);
        assert_eq!(parsed.account.balance, 10000.5);
        assert_eq!(parsed.account.trade_mode, "demo");
        assert_eq!(parsed.account.margin_mode, "hedging");

        assert_eq!(parsed.symbols.len(), 1);
        let spec = &parsed.symbols[0];
        assert_eq!(spec.symbol, "XAUUSD");
        assert_eq!(spec.contract_size, 100.0);
        assert_eq!(spec.volume_min, 0.01);

        assert_eq!(parsed.positions.len(), 1);
        let pos = &parsed.positions[0];
        assert_eq!(pos.ticket, 111);
        assert_eq!(pos.side, "long");
        assert_eq!(pos.magic, 20260907);

        assert_eq!(parsed.orders.len(), 1);
        assert_eq!(parsed.orders[0].kind, "buy_limit");

        assert_eq!(parsed.bars.len(), 1);
        assert_eq!(parsed.bars[0].0, "XAUUSD:M15");
        assert_eq!(parsed.bars[0].1.len(), 2);
        assert_eq!(parsed.bars[0].1[0].time, 1699999100);
        assert_eq!(parsed.bars[0].1[1].close, 1930.5);

        assert_eq!(parsed.results.len(), 1);
        assert!(parsed.results[0].ok);
        assert_eq!(parsed.results[0].data, Value::from("999"));
    }

    #[test]
    fn test_parse_res_positional_format() {
        // PABridge QueueResult 实际发送的位置格式: id|ok|msg|data
        let body = "T2A 1\n\
                    RES id=cmd_a|1|LIMIT sell XAUUSDm 5.65 @ 4398.50: retcode=10009 done|431245678\n\
                    RES id=cmd_b|0|品种 XAUUSDm 在当前 MT5 终端不存在|\n\
                    END\n";
        let parsed = parse_sync(body).unwrap();
        assert_eq!(parsed.results.len(), 2);

        let r0 = &parsed.results[0];
        assert_eq!(r0.id, "cmd_a");
        assert!(r0.ok, "成功回执必须解析为 ok=true");
        assert_eq!(r0.msg, "LIMIT sell XAUUSDm 5.65 @ 4398.50: retcode=10009 done");
        assert_eq!(r0.data, Value::from("431245678"));

        let r1 = &parsed.results[1];
        assert_eq!(r1.id, "cmd_b");
        assert!(!r1.ok);
        assert_eq!(r1.msg, "品种 XAUUSDm 在当前 MT5 终端不存在");
        assert!(r1.data.is_null());
    }

    #[test]
    fn test_serialize_commands_roundtrip_shape() {
        let cmds = vec![
            Command {
                id: "c1".to_string(),
                action: CommandAction::Market {
                    symbol: "XAUUSD".to_string(),
                    side: "buy".to_string(),
                    volume: 0.1,
                    sl: 1925.0,
                    tp: 1940.0,
                    comment: "pa123".to_string(),
                    magic: 20260907,
                },
            },
            Command {
                id: "c2".to_string(),
                action: CommandAction::ModifySltp { ticket: 111, sl: Some(1935.5), tp: None },
            },
            Command {
                id: "c3".to_string(),
                action: CommandAction::Subscribe { series: "XAUUSD:M15".to_string(), bars: 400 },
            },
        ];
        let text = serialize_commands(&cmds);
        assert!(text.starts_with("A2T 1\n"));
        assert!(text.contains("action=MARKET|symbol=XAUUSD|side=buy|volume=0.1|price=|sl=1925|tp=1940"));
        assert!(text.contains("action=MODIFY|ticket=111|sl=1935.5|tp="));
        assert!(text.contains("action=SUBSCRIBE|series=XAUUSD:M15|bars=400"));
        assert!(text.ends_with("END\n"));
    }

    #[test]
    fn test_parse_empty_and_garbage() {
        assert!(parse_sync("").is_err());
        assert!(parse_sync("GARBAGE 1\nEND\n").is_err());
        assert!(parse_sync("T2A 9\nEND\n").is_err());
    }

    #[test]
    fn test_sanitize_field() {
        assert_eq!(sanitize_field("a|b=c\nd"), "a／b＝c d");
    }
}
