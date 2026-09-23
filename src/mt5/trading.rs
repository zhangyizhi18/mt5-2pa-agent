use crate::data::timeframe_to_seconds;
use crate::mt5::client::MT5Bridge;
use crate::mt5::types::SymbolSpec;
use crate::util::timefmt::now_local_ms;
use anyhow::{anyhow, Result};
use parking_lot::{Mutex, ReentrantMutex};

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::str::FromStr;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// f64 -> Decimal 的安全转换：走最短字符串表示，避免二进制浮点伪影
/// (例如 from_f64_retain(0.01) 会得到 0.0100000000000000002081...)。
fn dec(v: f64) -> Decimal {
    Decimal::from_str(&format!("{}", v)).unwrap_or(Decimal::ZERO)
}

/// 审计与溯源标签。
pub const BROKER_TAG: &str = "PA-MT5-BRIDGE";

/// 本代理生成订单注释的前缀（EA 端仅按 magic 过滤，此注释用于人工辨识）。
pub const PA_CLIENT_ORDER_PREFIX: &str = "pa";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionResult {
    pub submitted: bool,
    pub signal_id: String,
    pub request: Value,
    pub response: Option<Value>,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub error_code: String,
    #[serde(default)]
    pub broker_tag: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub id: String,
    pub timestamp_ms: i64,
    pub submitted: bool,
    pub signal_id: String,
    pub instrument: String,
    pub timeframe: String,
    pub direction: String,
    pub order_type: String,
    pub confidence: Option<Value>,
    pub size: Option<Value>,
    pub price: Option<Value>,
    pub stop_loss_price: Option<Value>,
    pub take_profit_price: Option<Value>,
    pub order_id: String,
    pub reason: String,
    pub error_code: String,
    pub broker_tag: String,
    #[serde(default)]
    pub deleted: bool,
}

#[derive(Debug, Clone)]
pub struct MT5TradeExecutor {
    bridge: MT5Bridge,
    default_order_size: Decimal,
    default_leverage: Decimal,
    block_new_entries_when_position_open: bool,
    confidence_threshold: u32,
    max_signal_age_seconds: u64,
    pub max_pending_bars: usize,
    audit_path: Option<PathBuf>,
    seen: Arc<Mutex<HashSet<String>>>,
    audit_lock: Arc<ReentrantMutex<()>>,
    pub auto_order_sizing: bool,
    pub risk_percent: Decimal,
    pub max_margin_percent: Decimal,
}

impl MT5TradeExecutor {
    pub fn new(
        bridge: MT5Bridge,
        default_order_size: f64,
        default_leverage: f64,
        block_new_entries_when_position_open: bool,
        confidence_threshold: u32,
        max_signal_age_seconds: u64,
        max_pending_bars: usize,
        audit_path: Option<PathBuf>,
        auto_order_sizing: bool,
        risk_percent: f64,
        max_margin_percent: f64,
    ) -> Self {
        let default_order_size = dec(default_order_size).max(Decimal::ZERO);
        let default_leverage = if default_leverage > 0.0 { dec(default_leverage) } else { Decimal::from(100) };
        let risk_percent = if risk_percent > 0.0 { dec(risk_percent) } else { Decimal::from(2) };
        let max_margin_percent = if max_margin_percent > 0.0 { dec(max_margin_percent) } else { Decimal::from(25) };

        let executor = Self {
            bridge,
            default_order_size,
            default_leverage,
            block_new_entries_when_position_open,
            confidence_threshold: confidence_threshold.min(100),
            max_signal_age_seconds: max_signal_age_seconds.max(5),
            max_pending_bars: max_pending_bars.max(1),
            audit_path,
            seen: Arc::new(Mutex::new(HashSet::new())),
            audit_lock: Arc::new(ReentrantMutex::new(())),
            auto_order_sizing,
            risk_percent,
            max_margin_percent,
        };
        executor.load_seen();
        executor
    }

    fn load_seen(&self) {
        if let Some(path) = &self.audit_path {
            if path.is_file() {
                if let Ok(file) = File::open(path) {
                    let reader = BufReader::new(file);
                    let mut seen = self.seen.lock();
                    for line in reader.lines().flatten() {
                        if let Ok(val) = serde_json::from_str::<Value>(&line) {
                            if val.get("submitted").and_then(|v| v.as_bool()).unwrap_or(false) {
                                if let Some(sig) = val.get("signal_id").and_then(|v| v.as_str()) {
                                    seen.insert(sig.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    pub fn generate_signal_id(
        symbol: &str,
        timeframe: &str,
        signal_ts_ms: i64,
        decision: &Value,
    ) -> String {
        let material = serde_json::json!({
            "inst_id": symbol,
            "timeframe": timeframe,
            "signal_ts_ms": signal_ts_ms,
            "order_direction": decision.get("order_direction"),
            "order_type": decision.get("order_type"),
            "entry_price": decision.get("entry_price"),
            "stop_loss_price": decision.get("stop_loss_price"),
            "take_profit_price": decision.get("take_profit_price"),
        });
        let s = material.to_string();
        let mut hasher = Sha256::new();
        hasher.update(s.as_bytes());
        let hex = hex::encode(hasher.finalize());
        hex[..24].to_string()
    }

    fn audit(&self, result: &ExecutionResult, symbol: &str, timeframe: &str, decision: &Value) {
        let path = match &self.audit_path {
            Some(p) => p,
            None => return,
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        let entry = serde_json::json!({
            "id": Uuid::new_v4().simple().to_string(),
            "ts_ms": now_ms,
            "inst_id": symbol,
            "timeframe": timeframe,
            "submitted": result.submitted,
            "signal_id": result.signal_id,
            "request": result.request,
            "response": result.response,
            "reason": result.reason,
            "error_code": result.error_code,
            "broker_tag": BROKER_TAG,
            "decision": {
                "order_direction": decision.get("order_direction"),
                "order_type": decision.get("order_type"),
                "entry_price": decision.get("entry_price"),
                "stop_loss_price": decision.get("stop_loss_price"),
                "take_profit_price": decision.get("take_profit_price"),
                "trade_confidence": decision.get("trade_confidence"),
            }
        });

        let _guard = self.audit_lock.lock();
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(file, "{}", entry);
        }
    }

    pub fn audit_history(&self, limit: usize) -> Vec<AuditEntry> {
        let path = match &self.audit_path {
            Some(p) => p,
            None => return Vec::new(),
        };
        if !path.is_file() {
            return Vec::new();
        }

        let _guard = self.audit_lock.lock();
        let file = match File::open(path) {
            Ok(f) => f,
            Err(_) => return Vec::new(),
        };

        let reader = BufReader::new(file);
        let mut entries = Vec::new();

        for line in reader.lines().flatten() {
            if let Ok(item) = serde_json::from_str::<Value>(&line) {
                if item.get("deleted").and_then(|v| v.as_bool()).unwrap_or(false) {
                    continue;
                }

                let req = item.get("request").cloned().unwrap_or(Value::Null);
                let resp = item.get("response").cloned().unwrap_or(Value::Null);
                let dec = item.get("decision").cloned().unwrap_or(Value::Null);

                let order_id = resp.get("ticket")
                    .or_else(|| resp.get("data"))
                    .and_then(|v| v.as_str().map(|s| s.to_string()).or_else(|| v.as_i64().map(|i| i.to_string())))
                    .unwrap_or_default();

                let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let timestamp_ms = item.get("ts_ms").and_then(|v| v.as_i64()).unwrap_or(0);
                let submitted = item.get("submitted").and_then(|v| v.as_bool()).unwrap_or(false);
                let signal_id = item.get("signal_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let instrument = req.get("symbol")
                    .or_else(|| item.get("inst_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let timeframe = item.get("timeframe").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let direction = req.get("side")
                    .or_else(|| dec.get("order_direction"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let order_type = req.get("kind")
                    .or_else(|| dec.get("order_type"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let confidence = dec.get("trade_confidence").cloned();
                let size = req.get("volume").cloned();
                let price = req.get("price").or_else(|| dec.get("entry_price")).cloned();
                let stop_loss_price = req.get("sl").or_else(|| dec.get("stop_loss_price")).cloned();
                let take_profit_price = req.get("tp").or_else(|| dec.get("take_profit_price")).cloned();
                let reason = item.get("reason")
                    .or_else(|| resp.get("msg"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let error_code = item.get("error_code").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let broker_tag = item.get("broker_tag").and_then(|v| v.as_str()).unwrap_or(BROKER_TAG).to_string();

                entries.push(AuditEntry {
                    id,
                    timestamp_ms,
                    submitted,
                    signal_id,
                    instrument,
                    timeframe,
                    direction,
                    order_type,
                    confidence,
                    size,
                    price,
                    stop_loss_price,
                    take_profit_price,
                    order_id,
                    reason,
                    error_code,
                    broker_tag,
                    deleted: false,
                });
            }
        }

        entries.sort_by(|a, b| b.timestamp_ms.cmp(&a.timestamp_ms));
        if entries.len() > limit {
            entries.truncate(limit);
        }
        entries
    }

    pub fn delete_audit_entry(&self, entry_id: &str) -> bool {
        let path = match &self.audit_path {
            Some(p) => p,
            None => return false,
        };
        if !path.is_file() || entry_id.is_empty() {
            return false;
        }

        let _guard = self.audit_lock.lock();
        let file = match File::open(path) {
            Ok(f) => f,
            Err(_) => return false,
        };

        let reader = BufReader::new(file);
        let mut output = Vec::new();
        let mut found = false;

        for line in reader.lines().flatten() {
            if let Ok(item) = serde_json::from_str::<Value>(&line) {
                let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let is_del = item.get("deleted").and_then(|v| v.as_bool()).unwrap_or(false);

                if !found && id == entry_id && !is_del {
                    found = true;
                    if item.get("submitted").and_then(|v| v.as_bool()).unwrap_or(false) {
                        let tombstone = serde_json::json!({
                            "id": entry_id,
                            "ts_ms": item.get("ts_ms"),
                            "submitted": true,
                            "signal_id": item.get("signal_id"),
                            "deleted": true,
                        });
                        output.push(tombstone.to_string());
                    }
                    continue;
                }
                output.push(line);
            }
        }

        if !found {
            return false;
        }

        if let Ok(mut out_file) = File::create(path) {
            for l in output {
                let _ = writeln!(out_file, "{}", l);
            }
            true
        } else {
            false
        }
    }

    fn validate_prices(&self, decision: &Value) -> Result<(Decimal, Decimal, Decimal)> {
        let entry_f = decision.get("entry_price").and_then(|v| v.as_f64()).ok_or_else(|| anyhow!("缺少入场价 (entry_price)"))?;
        let stop_f = decision.get("stop_loss_price").and_then(|v| v.as_f64()).ok_or_else(|| anyhow!("缺少止损价 (stop_loss_price)"))?;
        let target_f = decision.get("take_profit_price").and_then(|v| v.as_f64()).ok_or_else(|| anyhow!("缺少止盈价 (take_profit_price)"))?;

        let entry = dec(entry_f);
        let stop = dec(stop_f);
        let target = dec(target_f);

        let direction = decision.get("order_direction").and_then(|v| v.as_str()).unwrap_or("");
        if direction == "做多" {
            if !(stop < entry && entry < target) {
                return Err(anyhow!("做多价格关系异常：必须满足 止损价 < 入场价 < 止盈价"));
            }
        } else if direction == "做空" {
            if !(target < entry && entry < stop) {
                return Err(anyhow!("做空价格关系异常：必须满足 止盈价 < 入场价 < 止损价"));
            }
        } else {
            return Err(anyhow!("订单方向必须为 做多 或 做空"));
        }

        Ok((entry, stop, target))
    }

    fn floor_step(value: Decimal, step: Decimal) -> Decimal {
        if step <= Decimal::ZERO { return value; }
        (value / step).floor() * step
    }

    fn round_tick(value: Decimal, tick: Decimal) -> Decimal {
        if tick <= Decimal::ZERO { return value; }
        (value / tick).round() * tick
    }

    /// 按风险比例与保证金上限计算下单手数（MT5 手数语义）。
    pub fn compute_order_size(
        &self,
        symbol: &str,
        spec: &SymbolSpec,
        entry: Decimal,
        stop: Decimal,
        equity: Decimal,
        free_margin: Decimal,
    ) -> Result<Decimal> {
        let lot_sz = { let s = dec(spec.volume_step.abs()); if s > Decimal::ZERO { s } else { Decimal::new(1, 2) } };
        let min_sz = dec(spec.volume_min.abs()).max(Decimal::ZERO);
        let max_sz = { let m = dec(spec.volume_max.abs()); if m > Decimal::ZERO { m } else { Decimal::from(100) } };
        let contract = { let c = dec(spec.contract_size.abs()); if c > Decimal::ZERO { c } else { Decimal::ONE } };

        // 1. 一手名义价值（以账户货币近似计，忽略报价币种换算）
        let notional_per_lot = contract * entry;

        // 2. 一手所需保证金：优先使用经纪商上报的初始保证金，否则按杠杆估算
        let lev = if self.default_leverage > Decimal::ZERO { self.default_leverage } else { Decimal::from(100) };
        let margin_initial = dec(spec.margin_initial.abs());
        let margin_per_lot = if margin_initial > Decimal::ZERO {
            margin_initial
        } else if notional_per_lot > Decimal::ZERO {
            notional_per_lot / lev
        } else {
            Decimal::ZERO
        };

        // 3. 保证金上限约束
        let margin_cap_percent = self.max_margin_percent.clamp(Decimal::ONE, Decimal::from(100)) / Decimal::from(100);
        let max_usable_margin = free_margin * margin_cap_percent;
        let max_lots_by_margin = if margin_per_lot > Decimal::ZERO {
            max_usable_margin / margin_per_lot
        } else {
            Decimal::ZERO
        };

        // 4. 单笔风险约束
        let risk_cap_percent = self.risk_percent.clamp(Decimal::new(1, 1), Decimal::from(20)) / Decimal::from(100);
        let allowed_risk = equity * risk_cap_percent;
        let stop_dist = (entry - stop).abs();
        let risk_per_lot = stop_dist * contract;
        let lots_by_risk = if risk_per_lot > Decimal::ZERO {
            allowed_risk / risk_per_lot
        } else {
            max_lots_by_margin
        };

        // 5. 目标手数
        let target = if self.auto_order_sizing {
            let mut lots = lots_by_risk.min(max_lots_by_margin);
            if lots < min_sz && max_lots_by_margin >= min_sz {
                lots = min_sz;
            }
            lots
        } else {
            let user_lots = self.default_order_size;
            if user_lots > max_lots_by_margin && max_lots_by_margin >= min_sz {
                tracing::warn!(
                    "用户设定的默认手数 {} 超出安全保证金承载上限 {:.4} (标的: {})，自动裁剪以防拒单。",
                    user_lots, max_lots_by_margin, symbol
                );
                max_lots_by_margin
            } else {
                user_lots
            }
        };

        let size = Self::floor_step(target.min(max_sz), lot_sz);

        // 6. 最小手数校验
        if size < min_sz {
            let min_margin_needed = min_sz * margin_per_lot;
            if free_margin < min_margin_needed {
                return Err(anyhow!(
                    "账户可用保证金 ({:.2} {}) 不足以开立最小手数 {} 手 (1手名义约 {:.2}，最小需 {:.2} 保证金，杠杆 {}x)。请入金或更换面值更小的品种。",
                    free_margin,
                    "账户货币",
                    min_sz,
                    notional_per_lot,
                    min_margin_needed,
                    lev
                ));
            } else {
                return Ok(min_sz);
            }
        }

        tracing::info!(
            "动态算量完成: 标的={}, 最终手数={}, 1手名义价值≈{:.2}, 所需保证金≈{:.2}, 账户可用={:.2}",
            symbol, size, notional_per_lot, size * margin_per_lot, free_margin
        );

        Ok(size)
    }

    /// 构造标准化下单意图（intent）。返回 (intent, kind)。
    pub async fn build_request(
        &self,
        symbol: &str,
        decision: &Value,
        signal_id: &str,
    ) -> Result<(Value, &'static str)> {
        let order_type = decision.get("order_type").and_then(|v| v.as_str()).unwrap_or("");
        if !["限价单", "突破单", "市价单"].contains(&order_type) {
            return Err(anyhow!("决策为不下单或不包含可执行订单"));
        }

        let confidence = decision.get("trade_confidence").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        if confidence < self.confidence_threshold {
            return Err(anyhow!("交易信心度 {}% 低于设定风控门槛 {}%", confidence, self.confidence_threshold));
        }

        let (entry, stop, target) = self.validate_prices(decision)?;

        let spec = self.bridge.get_symbol_spec(symbol).await?;

        let equity = dec(self.bridge.get_account().equity);
        let free_margin = dec(self.bridge.get_account().free_margin);
        let size = self.compute_order_size(symbol, &spec, entry, stop, equity, free_margin)?;

        let direction = decision.get("order_direction").and_then(|v| v.as_str()).unwrap_or("");
        let side = if direction == "做多" { "buy" } else { "sell" };

        let tick = { let t = dec(if spec.tick_size > 0.0 { spec.tick_size } else { spec.point }); if t > Decimal::ZERO { t } else { Decimal::new(1, 4) } };
        let entry_s = Self::round_tick(entry, tick);
        let stop_s = Self::round_tick(stop, tick);
        let target_s = Self::round_tick(target, tick);

        let comment = format!("{}{}", PA_CLIENT_ORDER_PREFIX, signal_id);

        let bid = spec.bid;
        let ask = spec.ask;

        // 订单种类映射：
        // - 市价单 -> market
        // - 突破单 -> 入场价穿越市场方向为 stop（买在价上/卖在价下），反之为 limit（回踩入场）
        // - 限价单 -> limit；若价格已穿越市场则转为 market 立即成交语义
        let kind: &'static str = match order_type {
            "市价单" => "market",
            "突破单" => {
                if side == "buy" {
                    if ask > 0.0 && entry_s < dec(ask) { "limit" } else { "stop" }
                } else if bid > 0.0 && entry_s > dec(bid) { "limit" } else { "stop" }
            }
            _ => {
                if side == "buy" {
                    if ask > 0.0 && entry_s >= dec(ask) { "market" } else { "limit" }
                } else if bid > 0.0 && entry_s <= dec(bid) { "market" } else { "limit" }
            }
        };

        let intent = serde_json::json!({
            "symbol": spec.symbol,
            "kind": kind,
            "side": side,
            "volume": size.normalize().to_string(),
            "price": if kind == "market" { serde_json::Value::Null } else { serde_json::json!(entry_s.normalize().to_string()) },
            "sl": stop_s.normalize().to_string(),
            "tp": target_s.normalize().to_string(),
            "comment": comment,
            "magic": self.bridge.magic(),
            "tag": BROKER_TAG,
        });

        Ok((intent, kind))
    }

    pub async fn execute(
        &self,
        symbol: &str,
        timeframe: &str,
        signal_ts_ms: i64,
        decision: &Value,
    ) -> ExecutionResult {
        let symbol_up = symbol.trim().to_uppercase();
        let signal_id = Self::generate_signal_id(&symbol_up, timeframe, signal_ts_ms, decision);

        let tf_seconds = timeframe_to_seconds(timeframe).unwrap_or(300);
        let bar_duration_ms = (tf_seconds as i64) * 1000;
        let bar_close_ts_ms = signal_ts_ms + bar_duration_ms;
        let now_ms = now_local_ms();

        let age_since_close_seconds = if now_ms > bar_close_ts_ms {
            ((now_ms - bar_close_ts_ms) as f64) / 1000.0
        } else {
            0.0
        };

        let max_age_allowed = ((self.max_pending_bars.max(1) as u64) * tf_seconds).max(self.max_signal_age_seconds) as f64;

        if age_since_close_seconds > max_age_allowed {
            let res = ExecutionResult {
                submitted: false,
                signal_id: signal_id.clone(),
                request: Value::Null,
                response: None,
                reason: format!("信号已过期 (K线闭合距今已过 {:.0} 秒，超过最大容许时效 {:.0} 秒)", age_since_close_seconds, max_age_allowed),
                error_code: String::new(),
                broker_tag: BROKER_TAG.to_string(),
            };
            self.audit(&res, &symbol_up, timeframe, decision);
            return res;
        }

        {
            let seen = self.seen.lock();
            if seen.contains(&signal_id) {
                return ExecutionResult {
                    submitted: false,
                    signal_id: signal_id.clone(),
                    request: Value::Null,
                    response: None,
                    reason: "重复信号 (当前K线周期已处理或挂单)".to_string(),
                    error_code: String::new(),
                    broker_tag: BROKER_TAG.to_string(),
                };
            }
        }

        let (request, kind) = match self.build_request(&symbol_up, decision, &signal_id).await {
            Ok(r) => r,
            Err(e) => {
                let res = ExecutionResult {
                    submitted: false,
                    signal_id: signal_id.clone(),
                    request: Value::Null,
                    response: None,
                    reason: e.to_string(),
                    error_code: String::new(),
                    broker_tag: BROKER_TAG.to_string(),
                };
                self.audit(&res, &symbol_up, timeframe, decision);
                return res;
            }
        };

        // 下单统一使用 EA 上报的规范品种名（如 XAUUSDm），避免大小写不匹配
        let trade_symbol = request
            .get("symbol")
            .and_then(|v| v.as_str())
            .unwrap_or(&symbol_up)
            .to_string();

        // 持仓互斥保护
        if self.block_new_entries_when_position_open {
            let has_position = self
                .bridge
                .get_positions(Some(&trade_symbol))
                .iter()
                .any(|p| p.volume.abs() > 1e-8);
            if has_position {
                let res = ExecutionResult {
                    submitted: false,
                    signal_id: signal_id.clone(),
                    request: request.clone(),
                    response: None,
                    reason: format!("{} 已存在活跃持仓，系统已启用持仓互斥保护（禁止同向加仓）", trade_symbol),
                    error_code: String::new(),
                    broker_tag: BROKER_TAG.to_string(),
                };
                self.audit(&res, &symbol_up, timeframe, decision);
                return res;
            }
        }

        // Cancel-Replace：撤销同品种本代理（magic 匹配）旧挂单，防止挂单堆积
        let old_orders = self.bridge.get_pending_orders(Some(&trade_symbol));
        for old in old_orders {
            if old.magic == self.bridge.magic() || old.comment.starts_with(PA_CLIENT_ORDER_PREFIX) {
                tracing::info!(
                    "[决策执行] Cancel-Replace 撤销旧挂单: #{} {} {} 手数={} 价格={}",
                    old.ticket, old.symbol, old.kind, old.volume, old.price
                );
                let _ = self.bridge.cancel_order(old.ticket).await;
            }
        }

        // 下单
        let volume: f64 = request.get("volume")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        let price: f64 = request.get("price").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let sl: f64 = request.get("sl").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let tp: f64 = request.get("tp").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let side = request.get("side").and_then(|v| v.as_str()).unwrap_or("buy");
        let comment = request.get("comment").and_then(|v| v.as_str()).unwrap_or("");

        tracing::info!(
            "[决策执行] AI 决策推送下单: signal_id={} 标的={} 类型={} 方向={} 手数={} 价格={} SL={} TP={} 备注={}",
            signal_id, trade_symbol, kind, side, volume, price, sl, tp, comment
        );

        let order_res = match kind {
            "market" => self.bridge.place_market(&trade_symbol, side, volume, sl, tp, comment).await,
            "limit" => self.bridge.place_limit(&trade_symbol, side, volume, price, sl, tp, comment).await,
            _ => self.bridge.place_stop(&trade_symbol, side, volume, price, sl, tp, comment).await,
        };

        match order_res {
            Ok(resp) => {
                {
                    let mut seen = self.seen.lock();
                    seen.insert(signal_id.clone());
                }
                tracing::info!(
                    "[决策执行] 下单成功: signal_id={} 标的={} 类型={} 票据={} 回执={}",
                    signal_id, trade_symbol, kind,
                    resp.data.as_str().unwrap_or(""),
                    resp.msg
                );
                let res = ExecutionResult {
                    submitted: true,
                    signal_id: signal_id.clone(),
                    request: request.clone(),
                    response: Some(serde_json::json!({
                        "ticket": resp.data.as_str().unwrap_or(""),
                        "msg": resp.msg,
                    })),
                    reason: String::new(),
                    error_code: String::new(),
                    broker_tag: BROKER_TAG.to_string(),
                };
                self.audit(&res, &symbol_up, timeframe, decision);
                res
            }
            Err(e) => {
                tracing::warn!(
                    "[决策执行] 下单失败: signal_id={} 标的={} 类型={} 方向={} 手数={} 价格={} 原因={}",
                    signal_id, trade_symbol, kind, side, volume, price, e
                );
                let res = ExecutionResult {
                    submitted: false,
                    signal_id: signal_id.clone(),
                    request: request.clone(),
                    response: None,
                    reason: e.to_string(),
                    error_code: String::new(),
                    broker_tag: BROKER_TAG.to_string(),
                };
                self.audit(&res, &symbol_up, timeframe, decision);
                res
            }
        }
    }
}
