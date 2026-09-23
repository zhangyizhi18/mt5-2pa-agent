use crate::config::paths::{records_dir, settings_json_path};
use crate::config::settings::Settings;
use crate::data::base::{KlineBar, PositionContext};
use crate::data::snapshot::{build_analysis_frame, build_live_frame, INDICATOR_WARMUP_BARS};
use crate::mt5::client::MT5Bridge;
use crate::mt5::trading::{AuditEntry, MT5TradeExecutor, BROKER_TAG};
use crate::mt5::types::{order_to_json, position_to_json};
use crate::orchestrator::two_stage::TwoStageOrchestrator;
use crate::records::history::{delete_record, list_record_paths, load_record};
use crate::util::mask::mask_secret;
use crate::web::sessions::{build_trading_session, TradingSession};
use anyhow::{anyhow, Result};
use chrono::{Timelike, Utc};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{error, info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaveEnvRequest {
    #[serde(default)]
    pub llm_api_key: String,
    #[serde(default = "default_llm_base_url")]
    pub llm_base_url: String,
    #[serde(default = "default_llm_model")]
    pub llm_model: String,
    #[serde(default)]
    pub llm_thinking: bool,

    #[serde(default = "default_trading_system")]
    pub trading_system: String,

    #[serde(default)]
    pub mt5_bridge_token: String,
    #[serde(default = "default_magic_number")]
    pub mt5_magic_number: i64,
    #[serde(default = "default_order_size")]
    pub mt5_default_order_size: f64,
    #[serde(default = "default_leverage")]
    pub mt5_default_leverage: f64,
    #[serde(default = "default_true")]
    pub mt5_auto_order_sizing: bool,
    #[serde(default = "default_risk_percent")]
    pub mt5_risk_percent: f64,
    #[serde(default = "default_max_margin_percent")]
    pub mt5_max_margin_percent: f64,
}

fn default_risk_percent() -> f64 { 2.0 }
fn default_max_margin_percent() -> f64 { 25.0 }

fn default_llm_base_url() -> String { "https://api.deepseek.com".to_string() }
fn default_llm_model() -> String { "deepseek-v4-flash".to_string() }
fn default_trading_system() -> String { "2pa".to_string() }
fn default_true() -> bool { true }
fn default_magic_number() -> i64 { 20260907 }
fn default_order_size() -> f64 { 0.1 }
fn default_leverage() -> f64 { 100.0 }

pub struct WebTradingService {
    pub settings: Arc<RwLock<Settings>>,
    pub bridge: Arc<RwLock<MT5Bridge>>,
    pub executor: Arc<RwLock<MT5TradeExecutor>>,
    pub orchestrator: Arc<RwLock<TwoStageOrchestrator>>,
    pub current_trading_system: Arc<RwLock<String>>,
    pub automation_enabled: Arc<RwLock<bool>>,
    pub automation_symbol: Arc<RwLock<String>>,
    pub automation_timeframe: Arc<RwLock<String>>,
    pub automation_session: Arc<RwLock<TradingSession>>,
    pub latest_analysis: Arc<RwLock<Option<Value>>>,
    pub last_closed_ts: Arc<RwLock<HashMap<(String, String), i64>>>,
    /// 权益曲线采样与持久化（方案 A）。
    pub equity_log: Arc<crate::web::equity_log::EquityLog>,
    /// Web 控制台账号与会话（admin / readonly）。
    pub auth: Arc<crate::web::auth::AuthService>,
}

impl WebTradingService {
    pub fn new(settings: Settings) -> Self {
        let bridge = MT5Bridge::new(
            &settings.mt5.bridge_token,
            settings.mt5.magic_number,
            settings.mt5.command_timeout_seconds,
            settings.mt5.bridge_stale_seconds,
        );

        let audit_path = Some(records_dir().join("trade_audit.jsonl"));
        let executor = MT5TradeExecutor::new(
            bridge.clone(),
            settings.mt5.default_order_size,
            settings.mt5.default_leverage,
            settings.mt5.block_new_entries_when_position_open,
            settings.general.decision_confidence_threshold,
            settings.mt5.max_signal_age_seconds,
            settings.mt5.max_pending_bars,
            audit_path,
            settings.mt5.auto_order_sizing,
            settings.mt5.risk_percent,
            settings.mt5.max_margin_percent,
        );

        let orchestrator = TwoStageOrchestrator::new(
            settings.clone(),
            records_dir(),
        );

        let session = build_trading_session(
            &settings.mt5.automation_session_preset,
            &settings.mt5.automation_session_timezone,
            &settings.mt5.automation_session_start,
            &settings.mt5.automation_session_end,
            Some(&settings.mt5.automation_session_weekdays),
        );

        let initial_system = settings.general.trading_system.clone();

        Self {
            settings: Arc::new(RwLock::new(settings)),
            bridge: Arc::new(RwLock::new(bridge)),
            executor: Arc::new(RwLock::new(executor)),
            orchestrator: Arc::new(RwLock::new(orchestrator)),
            current_trading_system: Arc::new(RwLock::new(initial_system)),
            automation_enabled: Arc::new(RwLock::new(false)),
            automation_symbol: Arc::new(RwLock::new("XAUUSD".to_string())),
            automation_timeframe: Arc::new(RwLock::new("15m".to_string())),
            automation_session: Arc::new(RwLock::new(session)),
            latest_analysis: Arc::new(RwLock::new(None)),
            last_closed_ts: Arc::new(RwLock::new(HashMap::new())),
            equity_log: Arc::new(crate::web::equity_log::EquityLog::new(records_dir().join("equity"))),
            auth: Arc::new(crate::web::auth::AuthService::new()),
        }
    }

    /// 获取桥接客户端快照（内部字段均为 Arc 包裹，clone 开销极低）。
    pub fn b(&self) -> MT5Bridge {
        self.bridge.read().clone()
    }

    /// 当前交易环境（demo/live/unknown），由 EA 上报的账户类型决定。
    pub fn trade_mode(&self) -> String {
        match self.b().get_account().trade_mode.as_str() {
            "real" => "live".to_string(),
            "demo" | "contest" => "demo".to_string(),
            _ => "unknown".to_string(),
        }
    }

    pub fn status(&self) -> Value {
        let settings = self.settings.read();
        let session = self.automation_session.read();
        let auto_enabled = *self.automation_enabled.read();
        let symbol = self.automation_symbol.read().clone();
        let timeframe = self.automation_timeframe.read().clone();
        let latest = self.latest_analysis.read().clone();
        let trading_system = self.current_trading_system.read().clone();
        let mode = self.trade_mode();
        let bridge_connected = self.b().is_connected();

        serde_json::json!({
            "ok": true,
            "mode": mode,
            "bridge_connected": bridge_connected,
            "bridge": self.b().terminal_summary(),
            "has_env_file": std::path::Path::new(".env").exists(),
            "is_ai_configured": settings.is_provider_configured(),
            "credentials_configured": bridge_connected,
            "auto_trading_enabled": auto_enabled,
            "live_execution_unlocked": mode != "live" || settings.mt5.live_trading_acknowledged,
            "can_execute": auto_enabled && bridge_connected,
            "broker_tag": BROKER_TAG,
            "magic_number": self.b().magic(),
            "symbol": symbol,
            "timeframe": timeframe,
            "trading_system": trading_system,
            "available_trading_systems": [
                {
                    "id": "2pa",
                    "name": "2PA 价格行为系统 (Al Brooks)",
                    "description": "基于经典价格行为学八态周期、EMA20 与二元决策树"
                },
                {
                    "id": "dog_walking",
                    "name": "🐕 遛狗系统 (SMA 14/170 均线回归)",
                    "description": "基于 14 狗绳与 170 主人均线偏离力学与均值回归"
                },
                {
                    "id": "adaptive",
                    "name": "🧠 智能自适应双引擎 (2PA + 遛狗)",
                    "description": "震荡与通道顺势走 2PA，极值偏离衰竭走遛狗均线回归"
                }
            ],
            "confidence_threshold": settings.general.decision_confidence_threshold,
            "default_order_size": settings.mt5.default_order_size,
            "default_leverage": settings.mt5.default_leverage,
            "auto_order_sizing": settings.mt5.auto_order_sizing,
            "risk_percent": settings.mt5.risk_percent,
            "max_margin_percent": settings.mt5.max_margin_percent,
            "block_new_entries_when_position_open": settings.mt5.block_new_entries_when_position_open,
            "max_pending_bars": settings.mt5.max_pending_bars,
            "automation_session": session.as_dict(Some(Utc::now())),
            "automation_session_presets": crate::web::sessions::session_preset_options(),
            "latest": latest,
        })
    }

    pub fn get_config(&self) -> Value {
        let settings = self.settings.read();
        let cur_sys = self.current_trading_system.read().clone();
        serde_json::json!({
            "has_env_file": std::path::Path::new(".env").exists(),
            "is_configured": settings.is_provider_configured(),
            "is_ai_configured": settings.is_provider_configured(),
            "bridge_connected": self.b().is_connected(),
            "llm_api_key": mask_secret(&settings.provider.api_key),
            "llm_base_url": settings.provider.base_url,
            "llm_model": settings.provider.model,
            "llm_thinking": settings.provider.thinking,
            "trading_system": cur_sys,
            "mt5_bridge_token": mask_secret(&settings.mt5.bridge_token),
            "mt5_magic_number": settings.mt5.magic_number,
            "mt5_default_order_size": settings.mt5.default_order_size,
            "mt5_default_leverage": settings.mt5.default_leverage,
            "mt5_auto_order_sizing": settings.mt5.auto_order_sizing,
            "mt5_risk_percent": settings.mt5.risk_percent,
            "mt5_max_margin_percent": settings.mt5.max_margin_percent,
        })
    }

    pub fn save_env_config(&self, req: &SaveEnvRequest) -> Result<Value> {
        // 掩码或空值表示"保持不变"，避免把界面上的掩码字符串写回 .env
        let old = self.settings.read().clone();
        let effective_llm_key = {
            let k = req.llm_api_key.trim();
            if k.is_empty() || k.contains('*') { old.provider.api_key.clone() } else { k.to_string() }
        };
        let effective_token = {
            let t = req.mt5_bridge_token.trim();
            if t.contains('*') { old.mt5.bridge_token.clone() } else { t.to_string() }
        };

        let content = format!(
            r#"# =============================================================================
# MT5 2PA Agent 运行时环境变量配置文件 (由系统向导自动生成)
# =============================================================================

# ------------------------------ 大语言模型配置 ------------------------------
LLM_API_KEY={}
LLM_BASE_URL={}
LLM_MODEL={}
LLM_THINKING={}
LLM_REASONING_EFFORT=high
LLM_CONTEXT_WINDOW=128000
LLM_STAGE_TIMEOUT_SECONDS=240

# ------------------------------ 交易系统选择 ------------------------------
TRADING_SYSTEM={}

# ------------------------------ MT5 桥接 ------------------------------
# 与 PABridge EA 输入参数 BridgeToken 保持一致（可留空表示不校验）
MT5_BRIDGE_TOKEN={}
# 与 PABridge EA 输入参数 MagicNumber 保持一致
MT5_MAGIC_NUMBER={}

# ------------------------------ 交易环境与开关 ------------------------------
# 保存配置时写回当前运行时的自动交易开关状态（避免被硬编码重置）
MT5_AUTO_TRADING_ENABLED={}
# 实盘风险声明确认状态（保持当前值，不由本表单修改）
MT5_LIVE_TRADING_ACKNOWLEDGED={}

# ------------------------------ 订单与风控 ------------------------------
# 固定下单手数（自动算量关闭时使用）
MT5_DEFAULT_ORDER_SIZE={}
# 仅用于保证金估算；MT5 实际杠杆以终端账户设置为准
MT5_DEFAULT_LEVERAGE={}
MT5_AUTO_ORDER_SIZING={}
MT5_RISK_PERCENT={}
MT5_MAX_MARGIN_PERCENT={}
MT5_BLOCK_NEW_ENTRIES_WHEN_POSITION_OPEN=true
MT5_MAX_SIGNAL_AGE_SECONDS=120
MT5_MAX_PENDING_BARS=3

# ------------------------------ 交易时段 ------------------------------
MT5_AUTOMATION_SESSION_PRESET=always
MT5_AUTOMATION_SESSION_TIMEZONE=UTC
"#,
            effective_llm_key,
            req.llm_base_url.trim(),
            req.llm_model.trim(),
            req.llm_thinking,
            req.trading_system.trim(),
            effective_token,
            req.mt5_magic_number,
            // 修复原参数错位：占位符 #8/#9 原本接到的是 !auto_order_sizing，
            // 导致 .env 里的实盘确认标志从未被正确写入
            *self.automation_enabled.read(),
            old.mt5.live_trading_acknowledged,
            req.mt5_default_order_size,
            req.mt5_default_leverage,
            req.mt5_auto_order_sizing,
            req.mt5_risk_percent,
            req.mt5_max_margin_percent,
        );

        std::fs::write(".env", content)?;
        info!("Successfully saved configuration to .env");

        // Reload new settings in-memory
        // 热加载专用路径：绕过进程环境变量中遗留的旧 .env 注入值，以新文件为准
        let config_path = settings_json_path();
        let new_settings = Settings::load_for_hot_reload(&config_path);

        let new_bridge = MT5Bridge::new(
            &new_settings.mt5.bridge_token,
            new_settings.mt5.magic_number,
            new_settings.mt5.command_timeout_seconds,
            new_settings.mt5.bridge_stale_seconds,
        );

        let audit_path = Some(records_dir().join("trade_audit.jsonl"));
        let new_executor = MT5TradeExecutor::new(
            new_bridge.clone(),
            new_settings.mt5.default_order_size,
            new_settings.mt5.default_leverage,
            new_settings.mt5.block_new_entries_when_position_open,
            new_settings.general.decision_confidence_threshold,
            new_settings.mt5.max_signal_age_seconds,
            new_settings.mt5.max_pending_bars,
            audit_path,
            new_settings.mt5.auto_order_sizing,
            new_settings.mt5.risk_percent,
            new_settings.mt5.max_margin_percent,
        );

        let new_orchestrator = TwoStageOrchestrator::new(
            new_settings.clone(),
            records_dir(),
        );

        *self.current_trading_system.write() = new_settings.general.trading_system.clone();
        *self.settings.write() = new_settings;
        *self.bridge.write() = new_bridge;
        *self.executor.write() = new_executor;
        *self.orchestrator.write() = new_orchestrator;

        Ok(self.get_config())
    }

    pub fn set_automation(
        &self,
        enabled: bool,
        symbol: &str,
        timeframe: &str,
        confirmation: &str,
        session_preset: Option<&str>,
        session_timezone: Option<&str>,
        session_start: Option<&str>,
        session_end: Option<&str>,
        session_weekdays: Option<&[u32]>,
        trading_system: Option<&str>,
    ) -> Result<Value> {
        let settings = self.settings.read();
        if enabled && !*self.automation_enabled.read() {
            let mode = self.trade_mode();
            let required = match mode.as_str() {
                "live" => "ENABLE LIVE",
                "demo" => "ENABLE DEMO",
                _ => {
                    return Err(anyhow!(
                        "MT5 桥接未连接或账户类型未知，无法确认交易环境。请先启动 MT5 终端并挂载 PABridge EA"
                    ))
                }
            };
            if confirmation.trim().to_uppercase() != required {
                return Err(anyhow!("confirmation must be {}", required));
            }
            if !self.b().is_connected() {
                return Err(anyhow!("MT5 桥接未连接，无法开启自动交易"));
            }
            if mode == "live" && !settings.mt5.live_trading_acknowledged {
                return Err(anyhow!("实盘交易未确认：请先在配置中确认实盘风险声明"));
            }
        }

        let cur_session = self.automation_session.read().clone();
        let session = build_trading_session(
            session_preset.unwrap_or(&cur_session.preset),
            session_timezone.unwrap_or(&cur_session.timezone_name),
            session_start.unwrap_or(&format!("{:02}:{:02}", cur_session.start.hour(), cur_session.start.minute())),
            session_end.unwrap_or(&format!("{:02}:{:02}", cur_session.end.hour(), cur_session.end.minute())),
            session_weekdays.or(Some(&cur_session.weekdays)),
        );

        if let Some(sys) = trading_system {
            if !sys.trim().is_empty() {
                *self.current_trading_system.write() = sys.trim().to_string();
            }
        }

        *self.automation_enabled.write() = enabled;
        *self.automation_symbol.write() = symbol.trim().to_uppercase();
        *self.automation_timeframe.write() = timeframe.to_string();
        *self.automation_session.write() = session;

        drop(settings);
        Ok(self.status())
    }

    pub async fn fetch_raw_candles(&self, symbol: &str, timeframe: &str, limit: usize) -> Result<Vec<KlineBar>> {
        self.b().get_candles(symbol, timeframe, limit).await
    }

    pub async fn instruments(&self, _inst_type: &str) -> Result<Vec<Value>> {
        Ok(self
            .b()
            .list_symbols()
            .iter()
            .map(|s| {
                serde_json::json!({
                    "symbol": s.symbol,
                    "description": s.description,
                    "digits": s.digits,
                    "trade_mode": s.trade_mode,
                })
            })
            .collect())
    }

    pub async fn candles(&self, symbol: &str, timeframe: &str, limit: usize) -> Result<Vec<KlineBar>> {
        let raw = self.fetch_raw_candles(symbol, timeframe, limit.max(10).min(300)).await?;
        let server_now = Some(self.b().server_now_ms());
        if let Some(frame) = build_live_frame(&raw, limit, symbol, timeframe, server_now) {
            Ok(frame.bars)
        } else {
            Ok(raw)
        }
    }

    fn find_position(&self, symbol: &str) -> Option<crate::mt5::types::BridgePosition> {
        self.b()
            .get_positions(Some(symbol))
            .into_iter()
            .find(|p| p.volume.abs() > 1e-8)
    }

    pub async fn account(&self) -> Result<Value> {
        if !self.b().is_connected() {
            return Ok(serde_json::json!({
                "configured": false,
                "summary": {},
                "equity_curve": [],
                "balances": [],
                "positions": [],
                "orders": [],
            }));
        }

        let account = self.b().get_account();
        let positions = self.b().get_positions(None);
        let orders = self.b().get_pending_orders(None);

        let upl: f64 = positions.iter().map(|p| p.profit + p.swap).sum();

        let summary = serde_json::json!({
            "total_equity_usd": account.equity,
            "available_equity_usd": account.free_margin,
            "unrealized_pnl": upl,
            "position_count": positions.len(),
            "pending_order_count": orders.len(),
            "currency": account.currency,
            "balance": account.balance,
            "margin_used": account.margin_used,
            "margin_level": account.margin_level,
            "leverage": account.leverage,
            "login": account.login,
            "server": account.server,
            "trade_mode": account.trade_mode,
        });

        Ok(serde_json::json!({
            "configured": true,
            "summary": summary,
            "balances": [crate::mt5::types::account_to_json(&account)],
            "positions": positions.iter().map(position_to_json).collect::<Vec<_>>(),
            "orders": orders.iter().map(order_to_json).collect::<Vec<_>>(),
        }))
    }

    /// 采样一次权益快照（桥接未连接返回 None，由后台循环调用并落盘）。
    pub fn sample_equity_now(&self) -> Option<crate::web::equity_log::EquityPoint> {
        if !self.b().is_connected() {
            return None;
        }
        let account = self.b().get_account();
        Some(crate::web::equity_log::EquityPoint {
            ts: Utc::now().timestamp_millis(),
            value: account.equity,
            balance: account.balance,
            unrealized: account.profit,
        })
    }

    /// 查询权益曲线（时间段 + 降采样）。
    pub fn equity_curve(
        &self,
        from_ms: i64,
        to_ms: i64,
        max_points: usize,
    ) -> Vec<crate::web::equity_log::EquityPoint> {
        self.equity_log.query(from_ms, to_ms, max_points)
    }

    pub async fn cancel_order(&self, _symbol: &str, ticket: i64) -> Result<Value> {
        let res = self.b().cancel_order(ticket).await?;
        Ok(serde_json::json!({ "cancelled": true, "ticket": ticket, "msg": res.msg }))
    }

    pub async fn cancel_all_orders(&self, symbol: Option<&str>) -> Result<usize> {
        let orders = self.b().get_pending_orders(symbol);
        let mut cancelled_count = 0;
        for ord in orders {
            // 只撤销本代理下发的挂单，避免误删人工挂单
            if ord.magic != self.b().magic() && !ord.comment.starts_with(crate::mt5::trading::PA_CLIENT_ORDER_PREFIX) {
                continue;
            }
            if self.b().cancel_order(ord.ticket).await.is_ok() {
                cancelled_count += 1;
            }
        }
        Ok(cancelled_count)
    }

    pub fn decision_records(&self, limit: usize) -> Vec<Value> {
        let paths = list_record_paths(&records_dir());
        let mut records = Vec::new();
        for p in paths.into_iter().take(limit) {
            let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
            if let Some(r) = load_record(&p) {
                let dec_obj = r.stage2_decision.as_ref();
                let inner_dec = dec_obj.and_then(|d| d.get("decision")).or(dec_obj);

                let direction = inner_dec.and_then(|d| d.get("order_direction")).and_then(|v| v.as_str()).unwrap_or("不下单").to_string();
                let order_type = inner_dec.and_then(|d| d.get("order_type")).and_then(|v| v.as_str()).unwrap_or("不下单").to_string();
                let confidence = inner_dec.and_then(|d| d.get("trade_confidence")).and_then(|v| v.as_u64());
                let entry_price = inner_dec.and_then(|d| d.get("entry_price")).and_then(|v| v.as_f64());
                let stop_loss_price = inner_dec.and_then(|d| d.get("stop_loss_price")).and_then(|v| v.as_f64());
                let take_profit_price = inner_dec.and_then(|d| d.get("take_profit_price")).and_then(|v| v.as_f64());
                let take_profit_price_2 = inner_dec.and_then(|d| d.get("take_profit_price_2")).and_then(|v| v.as_f64());
                let estimated_win_rate = inner_dec.and_then(|d| d.get("estimated_win_rate")).and_then(|v| v.as_f64());
                let reasoning = inner_dec.and_then(|d| d.get("reasoning")).and_then(|v| v.as_str()).unwrap_or("").to_string();
                let exception_str = r.exception.as_ref().map(|e| e.to_string());

                let item = serde_json::json!({
                    "id": stem,
                    "symbol": r.meta.symbol,
                    "timeframe": r.meta.timeframe,
                    "trading_system": r.meta.trading_system,
                    "timestamp_ms": r.meta.timestamp_local_ms,
                    "timestamp_iso": r.meta.timestamp_local_iso,
                    "direction": direction,
                    "order_type": order_type,
                    "confidence": confidence,
                    "entry_price": entry_price,
                    "stop_loss_price": stop_loss_price,
                    "take_profit_price": take_profit_price,
                    "take_profit_price_2": take_profit_price_2,
                    "estimated_win_rate": estimated_win_rate,
                    "reasoning": reasoning,
                    "exception": exception_str,
                    "meta": r.meta,
                    "stage1_diagnosis": r.stage1_diagnosis,
                    "stage2_decision": r.stage2_decision,
                    "usage": r.usage_total,
                });
                records.push(item);
            }
        }
        records
    }

    pub fn delete_decision_record(&self, record_id: &str) -> bool {
        delete_record(&records_dir(), record_id)
    }

    pub fn trade_records(&self, limit: usize) -> Vec<AuditEntry> {
        self.executor.read().audit_history(limit)
    }

    pub fn delete_trade_record(&self, record_id: &str) -> bool {
        self.executor.read().delete_audit_entry(record_id)
    }

    pub async fn analyze(
        &self,
        symbol: &str,
        timeframe: &str,
        bar_count: usize,
        execute: bool,
        system_override: Option<&str>,
    ) -> Result<Value> {
        let system = match system_override {
            Some(s) if !s.trim().is_empty() => {
                let s_clean = s.trim().to_string();
                *self.current_trading_system.write() = s_clean.clone();
                s_clean
            }
            _ => self.current_trading_system.read().clone(),
        };

        let fetch_limit = (bar_count + INDICATOR_WARMUP_BARS + 20).min(300).max(100);
        let raw_bars = self.fetch_raw_candles(symbol, timeframe, fetch_limit).await?;
        let server_now = Some(self.b().server_now_ms());
        let frame = build_analysis_frame(&raw_bars, bar_count, symbol, timeframe, server_now)
            .ok_or_else(|| anyhow!("not enough closed MT5 candles to build {}-bar analysis", bar_count))?;

        // 1. 实时获取当前品种的活跃持仓状态与生效中的止盈止损
        let mut pos_ctx = PositionContext {
            has_position: false,
            symbol: symbol.to_string(),
            pos_side: "none".to_string(),
            pos_size: "0".to_string(),
            mgn_mode: "net".to_string(),
            ..Default::default()
        };

        if let Some(p) = self.find_position(symbol) {
            let account = self.b().get_account();
            pos_ctx.has_position = true;
            pos_ctx.pos_side = p.side.clone();
            pos_ctx.pos_size = p.volume.abs().to_string();
            pos_ctx.open_avg_px = Some(p.price_open);
            pos_ctx.mark_px = Some(p.price_current);
            pos_ctx.unrealized_pnl = Some(p.profit + p.swap);
            // 浮盈比例按估算保证金近似（未实现盈亏 / (名义价值/杠杆)）
            let spec = self.b().get_symbol_spec(symbol).await.ok();
            let contract = spec.as_ref().map(|s| s.contract_size).unwrap_or(1.0).max(1.0);
            let lev = if account.leverage > 0 { account.leverage as f64 } else { 100.0 };
            let margin_est = p.volume * contract * p.price_open.abs() / lev;
            if margin_est > 1e-8 {
                pos_ctx.unrealized_pnl_ratio = Some((p.profit + p.swap) / margin_est * 100.0);
            }
            pos_ctx.leverage = Some(lev);
            pos_ctx.mgn_mode = account.margin_mode.clone();
            pos_ctx.open_time_ms = Some(p.time * 1000);
            if p.sl > 0.0 {
                pos_ctx.current_sl = Some(p.sl);
            }
            if p.tp > 0.0 {
                pos_ctx.current_tp = Some(p.tp);
            }
            pos_ctx.algo_id = Some(p.ticket.to_string());
        }

        // 2. 获取高时间框架 (HTF) 宏观共振背景
        let htf_tf = if timeframe == "1m" || timeframe == "3m" || timeframe == "5m" || timeframe == "15m" {
            Some("1h")
        } else if timeframe == "30m" || timeframe == "1h" {
            Some("4h")
        } else {
            None
        };

        let mut htf_context_str = None;
        if let Some(htf) = htf_tf {
            if let Ok(htf_bars) = self.fetch_raw_candles(symbol, htf, 40).await {
                if let Some(htf_frame) = build_analysis_frame(&htf_bars, 20, symbol, htf, server_now) {
                    if let Some(latest) = htf_frame.bars.first() {
                        let htf_close = latest.close;
                        let htf_ema20 = htf_frame.indicators.ema20.first().copied().unwrap_or(0.0);
                        let htf_sma170 = htf_frame.indicators.sma170.first().copied().unwrap_or(0.0);
                        let htf_trend = if htf_ema20 > 0.0 {
                            if htf_close > htf_ema20 { "偏多 (Bullish, 位于 HTF EMA20 之上)" } else { "偏空 (Bearish, 位于 HTF EMA20 之下)" }
                        } else {
                            "中性震荡"
                        };
                        htf_context_str = Some(format!(
                            "- **HTF 周期**: {}\n\
                             - **最新收盘价**: {:.4}\n\
                             - **HTF EMA20**: {:.4}\n\
                             - **HTF SMA170**: {:.4}\n\
                             - **宏观格局偏向**: {}\n\
                             - **共振交易指引**: 顺大做小。低级别入场信号若与 HTF 趋势共振（如 15m 多单 + 1H 偏多），期望值显著提高；若逆 HTF 趋势，必须严守短线与快速保本原则！",
                            htf, htf_close, htf_ema20, htf_sma170, htf_trend
                        ));
                    }
                }
            }
        }

        let record = {
            let orch = self.orchestrator.read().clone();
            orch.run_analysis_with_system_and_pos(&frame, &system, Some(&pos_ctx), htf_context_str.as_deref()).await?
        };

        let mut execution_res = Value::Null;
        if execute {
            if let Some(dec_wrap) = &record.stage2_decision {
                let dec = dec_wrap.get("decision").unwrap_or(dec_wrap);
                let order_type = dec.get("order_type").and_then(|v| v.as_str()).unwrap_or("");
                let action = dec.get("action").and_then(|v| v.as_str()).unwrap_or("");

                if ["限价单", "突破单", "市价单"].contains(&order_type) || action == "OPEN" {
                    let sig_ts = frame.bars.first().map(|b| b.ts_open).unwrap_or(0);
                    let executor = self.executor.read().clone();
                    let result = executor.execute(symbol, timeframe, sig_ts, dec).await;
                    execution_res = serde_json::to_value(result).unwrap_or(Value::Null);
                } else if order_type == "平仓" || action == "CLOSE_EARLY" {
                    match self.find_position(symbol) {
                        Some(p) => {
                            info!("Executing CLOSE_EARLY for {} (ticket {})...", symbol, p.ticket);
                            match self.b().close_position(p.ticket).await {
                                Ok(res) => {
                                    execution_res = serde_json::json!({
                                        "submitted": true,
                                        "action": "CLOSE_EARLY",
                                        "symbol": symbol,
                                        "reason": "AI 主动平仓 (CLOSE_EARLY) 离场成功",
                                        "response": serde_json::json!({ "ticket": p.ticket, "msg": res.msg })
                                    });
                                }
                                Err(e) => {
                                    execution_res = serde_json::json!({
                                        "submitted": false,
                                        "action": "CLOSE_EARLY",
                                        "symbol": symbol,
                                        "reason": format!("AI 主动平仓失败: {}", e)
                                    });
                                }
                            }
                        }
                        None => {
                            execution_res = serde_json::json!({
                                "submitted": false,
                                "action": "CLOSE_EARLY",
                                "symbol": symbol,
                                "reason": "当前无持仓，无需执行平仓"
                            });
                        }
                    }
                } else if order_type == "修改止损" || action == "MOVE_STOP_LOSS" {
                    let new_sl = dec.get("new_stop_loss_price").and_then(|v| v.as_f64())
                        .or_else(|| dec.get("stop_loss_price").and_then(|v| v.as_f64()));

                    if let Some(n_sl) = new_sl {
                        if pos_ctx.has_position {
                            // 铁律校验：单向移损（多单只能上移，空单只能下移）
                            let is_long = pos_ctx.pos_side == "long";
                            let is_valid_trailing = match pos_ctx.current_sl {
                                Some(cur_sl) => {
                                    if is_long { n_sl > cur_sl } else { n_sl < cur_sl }
                                }
                                None => true,
                            };

                            if is_valid_trailing {
                                info!("Executing MOVE_STOP_LOSS for {} to {}...", symbol, n_sl);
                                let ticket: i64 = pos_ctx.algo_id.as_deref().and_then(|s| s.parse().ok()).unwrap_or(0);
                                match self.b().modify_position_sltp(ticket, Some(n_sl), None).await {
                                    Ok(res) => {
                                        execution_res = serde_json::json!({
                                            "submitted": true,
                                            "action": "MOVE_STOP_LOSS",
                                            "symbol": symbol,
                                            "new_stop_loss": n_sl,
                                            "reason": format!("已成功修改 MT5 持仓止损至 {}", n_sl),
                                            "response": serde_json::json!({ "ticket": ticket, "msg": res.msg })
                                        });
                                    }
                                    Err(e) => {
                                        execution_res = serde_json::json!({
                                            "submitted": false,
                                            "action": "MOVE_STOP_LOSS",
                                            "symbol": symbol,
                                            "reason": format!("设置保护止损失败: {}", e)
                                        });
                                    }
                                }
                            } else {
                                execution_res = serde_json::json!({
                                    "submitted": false,
                                    "action": "MOVE_STOP_LOSS",
                                    "symbol": symbol,
                                    "reason": format!("拒绝逆向扩大止损扛单！当前止损: {:?}, 目标止损: {}", pos_ctx.current_sl, n_sl)
                                });
                            }
                        } else {
                            execution_res = serde_json::json!({
                                "submitted": false,
                                "action": "MOVE_STOP_LOSS",
                                "symbol": symbol,
                                "reason": "当前无持仓，无法移动止损"
                            });
                        }
                    }
                } else if order_type == "修改止盈" || action == "MOVE_TAKE_PROFIT" || action == "TRAILING_TAKE_PROFIT" {
                    let new_tp = dec.get("new_take_profit_price").and_then(|v| v.as_f64())
                        .or_else(|| dec.get("take_profit_price").and_then(|v| v.as_f64()));

                    if let Some(n_tp) = new_tp {
                        if pos_ctx.has_position {
                            info!("Executing MOVE_TAKE_PROFIT for {} to {}...", symbol, n_tp);
                            let ticket: i64 = pos_ctx.algo_id.as_deref().and_then(|s| s.parse().ok()).unwrap_or(0);
                            match self.b().modify_position_sltp(ticket, None, Some(n_tp)).await {
                                Ok(res) => {
                                    execution_res = serde_json::json!({
                                        "submitted": true,
                                        "action": "MOVE_TAKE_PROFIT",
                                        "symbol": symbol,
                                        "new_take_profit": n_tp,
                                        "reason": format!("已成功动态修改 MT5 持仓止盈至 {}", n_tp),
                                        "response": serde_json::json!({ "ticket": ticket, "msg": res.msg })
                                    });
                                }
                                Err(e) => {
                                    execution_res = serde_json::json!({
                                        "submitted": false,
                                        "action": "MOVE_TAKE_PROFIT",
                                        "symbol": symbol,
                                        "reason": format!("修改止盈失败: {}", e)
                                    });
                                }
                            }
                        } else {
                            execution_res = serde_json::json!({
                                "submitted": false,
                                "action": "MOVE_TAKE_PROFIT",
                                "symbol": symbol,
                                "reason": "当前无持仓，无法移动止盈"
                            });
                        }
                    }
                } else if order_type == "修改止盈止损" || action == "MOVE_SL_TP" {
                    let new_sl = dec.get("new_stop_loss_price").and_then(|v| v.as_f64())
                        .or_else(|| dec.get("stop_loss_price").and_then(|v| v.as_f64()));
                    let new_tp = dec.get("new_take_profit_price").and_then(|v| v.as_f64())
                        .or_else(|| dec.get("take_profit_price").and_then(|v| v.as_f64()));

                    if pos_ctx.has_position {
                        if let Some(ticket_str) = &pos_ctx.algo_id {
                            if let Ok(ticket) = ticket_str.parse::<i64>() {
                                if let Ok(res) = self.b().modify_position_sltp(ticket, new_sl, new_tp).await {
                                    execution_res = serde_json::json!({
                                        "submitted": true,
                                        "action": "MOVE_SL_TP",
                                        "symbol": symbol,
                                        "new_sl": new_sl,
                                        "new_tp": new_tp,
                                        "reason": "已成功同步更新 MT5 持仓止损与止盈",
                                        "response": serde_json::json!({ "ticket": ticket, "msg": res.msg })
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }

        let system_name = if system == "dog_walking" {
            "🐕 遛狗系统 (SMA 14/170 均线回归)"
        } else if system == "adaptive" {
            "🧠 智能自适应双引擎 (2PA + 遛狗)"
        } else {
            "2PA 价格行为系统 (Al Brooks)"
        };

        let output = serde_json::json!({
            "symbol": symbol,
            "timeframe": timeframe,
            "trading_system": system,
            "system_name": system_name,
            "signal_bar_ts": frame.bars.first().map(|b| b.ts_open).unwrap_or(0),
            "position_context": pos_ctx,
            "stage1": record.stage1_diagnosis,
            "stage2": record.stage2_decision,
            "decision": record.stage2_decision.as_ref().and_then(|d| d.get("decision")),
            "execution": execution_res,
            "usage": record.usage_total,
        });

        *self.latest_analysis.write() = Some(output.clone());
        Ok(output)
    }

    pub async fn automation_tick(&self) -> Result<()> {
        let auto_enabled = *self.automation_enabled.read();
        let session = self.automation_session.read().clone();
        if !auto_enabled || !session.is_open_at(Some(Utc::now())) {
            return Ok(());
        }

        let symbol = self.automation_symbol.read().clone();
        let timeframe = self.automation_timeframe.read().clone();

        let raw = match self.fetch_raw_candles(&symbol, &timeframe, 3).await {
            Ok(b) => b,
            Err(e) => {
                warn!("Automation tick failed to fetch candles: {}", e);
                return Ok(());
            }
        };

        if let Some(closed) = raw.iter().find(|b| b.closed) {
            let key = (symbol.clone(), timeframe.clone());
            let last_ts = self.last_closed_ts.read().get(&key).copied().unwrap_or(0);
            if last_ts == closed.ts_open {
                return Ok(());
            }

            self.last_closed_ts.write().insert(key, closed.ts_open);
            info!("New closed bar detected on {} ({}), triggering analysis...", symbol, timeframe);

            let bar_count = self.settings.read().general.analysis_bar_count;
            let system = self.current_trading_system.read().clone();
            if let Err(e) = self.analyze(&symbol, &timeframe, bar_count, true, Some(&system)).await {
                error!("Automation analysis error: {}", e);
            }
        }
        Ok(())
    }

    /// MT5 品种规格换算（合约计算器）。
    pub async fn get_contract_specs(&self, query_id: Option<&str>) -> Result<Value> {
        let specs = self.b().list_symbols();

        let popular_keys = [
            "XAUUSD", "XAGUSD", "EURUSD", "GBPUSD", "USDJPY", "USDCNH",
            "BTCUSD", "ETHUSD", "US30", "NAS100", "SPX500", "GER40", "USOIL", "UKOIL",
        ];

        let mut all_specs = Vec::new();
        let query_upper = query_id.map(|q| q.trim().to_uppercase()).unwrap_or_default();

        for spec in specs {
            if spec.symbol.is_empty() {
                continue;
            }

            if !query_upper.is_empty() && !spec.symbol.contains(&query_upper) && !spec.description.to_uppercase().contains(&query_upper) {
                continue;
            }

            let price = if spec.last > 0.0 { spec.last } else if spec.bid > 0.0 { spec.bid } else { spec.ask };
            let notional_per_lot = spec.contract_size * price;

            let is_popular = popular_keys.contains(&spec.symbol.as_str());

            all_specs.push(serde_json::json!({
                "symbol": spec.symbol,
                "description": spec.description,
                "contract_size": spec.contract_size,
                "volume_min": spec.volume_min,
                "volume_step": spec.volume_step,
                "volume_max": spec.volume_max,
                "tick_size": spec.tick_size,
                "digits": spec.digits,
                "bid": spec.bid,
                "ask": spec.ask,
                "last_price": price,
                "notional_per_lot": notional_per_lot,
                "trade_mode": spec.trade_mode,
                "is_popular": is_popular,
            }));
        }

        all_specs.sort_by(|a, b| {
            let a_pop = a.get("is_popular").and_then(|v| v.as_bool()).unwrap_or(false);
            let b_pop = b.get("is_popular").and_then(|v| v.as_bool()).unwrap_or(false);
            b_pop.cmp(&a_pop).then_with(|| {
                let a_id = a.get("symbol").and_then(|v| v.as_str()).unwrap_or("");
                let b_id = b.get("symbol").and_then(|v| v.as_str()).unwrap_or("");
                a_id.cmp(b_id)
            })
        });

        Ok(serde_json::json!({
            "total": all_specs.len(),
            "popular_count": popular_keys.len(),
            "specs": all_specs,
        }))
    }
}
