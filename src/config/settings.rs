use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AIProviderSettings {
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_false")]
    pub thinking: bool,
    #[serde(default = "default_reasoning_effort")]
    pub reasoning_effort: String,
    #[serde(default = "default_context_window")]
    pub context_window: usize,
    #[serde(default = "default_stage_timeout_seconds")]
    pub stage_timeout_seconds: u64,
}

fn default_model() -> String { "deepseek-v4-flash".to_string() }
fn default_base_url() -> String { "https://api.deepseek.com".to_string() }
fn default_false() -> bool { false }
fn default_true() -> bool { true }
fn default_reasoning_effort() -> String { "high".to_string() }
fn default_context_window() -> usize { 128_000 }
fn default_stage_timeout_seconds() -> u64 { 240 }

impl Default for AIProviderSettings {
    fn default() -> Self {
        Self {
            model: default_model(),
            base_url: default_base_url(),
            api_key: String::new(),
            thinking: false,
            reasoning_effort: default_reasoning_effort(),
            context_window: default_context_window(),
            stage_timeout_seconds: default_stage_timeout_seconds(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneralSettings {
    #[serde(default = "default_analysis_bar_count")]
    pub analysis_bar_count: usize,
    #[serde(default = "default_confidence_threshold")]
    pub decision_confidence_threshold: u32,
    #[serde(default = "default_decision_stance")]
    pub decision_stance: String,
    #[serde(default = "default_trading_system")]
    pub trading_system: String,
    #[serde(default)]
    pub enable_next_bar_prediction: bool,
    #[serde(default = "default_cooldown_bars")]
    pub structure_flip_cooldown_bars: usize,
}

fn default_analysis_bar_count() -> usize { 100 }
fn default_confidence_threshold() -> u32 { 40 }
fn default_decision_stance() -> String { "balanced".to_string() }
fn default_trading_system() -> String { "2pa".to_string() }
fn default_cooldown_bars() -> usize { 3 }

impl Default for GeneralSettings {
    fn default() -> Self {
        Self {
            analysis_bar_count: default_analysis_bar_count(),
            decision_confidence_threshold: default_confidence_threshold(),
            decision_stance: default_decision_stance(),
            trading_system: default_trading_system(),
            enable_next_bar_prediction: false,
            structure_flip_cooldown_bars: default_cooldown_bars(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptSettings {
    #[serde(default)]
    pub stage2_load_full_strategy_library: bool,
    #[serde(default)]
    pub experience_max_entries: usize,
    #[serde(default = "default_experience_max_chars")]
    pub experience_max_chars_per_entry: usize,
    #[serde(default = "default_true")]
    pub stage1_inject_pattern_briefs: bool,
}

fn default_experience_max_chars() -> usize { 400 }

impl Default for PromptSettings {
    fn default() -> Self {
        Self {
            stage2_load_full_strategy_library: false,
            experience_max_entries: 0,
            experience_max_chars_per_entry: default_experience_max_chars(),
            stage1_inject_pattern_briefs: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationSettings {
    #[serde(default = "default_normalization_mode")]
    pub normalization_mode: String,
    #[serde(default)]
    pub stage1_coherence_checks: bool,
    #[serde(default)]
    pub stage2_coherence_checks: bool,
    #[serde(default)]
    pub trace_semantic_checks: bool,
    #[serde(default)]
    pub strict_bar_by_bar_features: bool,
    #[serde(default)]
    pub disable_truncation_repair: bool,
    #[serde(default = "default_true")]
    pub retry_enabled: bool,
    #[serde(default = "default_retry_max")]
    pub retry_max: usize,
    #[serde(default = "default_retry_max_semantic")]
    pub retry_max_semantic: usize,
    #[serde(default = "default_true")]
    pub retry_stage2: bool,
}

fn default_normalization_mode() -> String { "lenient".to_string() }
fn default_retry_max() -> usize { 3 }
fn default_retry_max_semantic() -> usize { 1 }

impl Default for ValidationSettings {
    fn default() -> Self {
        Self {
            normalization_mode: default_normalization_mode(),
            stage1_coherence_checks: false,
            stage2_coherence_checks: false,
            trace_semantic_checks: false,
            strict_bar_by_bar_features: false,
            disable_truncation_repair: false,
            retry_enabled: true,
            retry_max: default_retry_max(),
            retry_max_semantic: default_retry_max_semantic(),
            retry_stage2: true,
        }
    }
}

/// MT5 桥接与交易参数。
///
/// MT5 终端侧的认证由 PABridge EA 完成（终端内已登录账户），
/// 这里只需要维护 Rust 服务与 EA 之间的共享令牌、魔术号与风控参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MT5Settings {
    /// Rust 服务与 EA 之间的可选共享令牌（EA 输入参数 BridgeToken）。
    #[serde(default)]
    pub bridge_token: String,
    /// 魔术号，EA 侧下单与撤单过滤使用（EA 输入参数 MagicNumber）。
    #[serde(default = "default_magic_number")]
    pub magic_number: i64,
    /// 等待 EA 回传指令结果的超时秒数。
    #[serde(default = "default_command_timeout")]
    pub command_timeout_seconds: u64,
    /// 超过该秒数未收到 EA 同步则视为桥接断开。
    #[serde(default = "default_bridge_stale")]
    pub bridge_stale_seconds: u64,
    #[serde(default)]
    pub auto_trading_enabled: bool,
    #[serde(default)]
    pub live_trading_acknowledged: bool,
    #[serde(default = "default_order_size")]
    pub default_order_size: f64,
    /// 仅用于保证金估算（MT5 杠杆为账户级，由终端设置）。
    #[serde(default = "default_leverage")]
    pub default_leverage: f64,
    #[serde(default = "default_true")]
    pub auto_order_sizing: bool,
    #[serde(default = "default_risk_percent")]
    pub risk_percent: f64,
    #[serde(default = "default_max_margin_percent")]
    pub max_margin_percent: f64,
    #[serde(default = "default_true")]
    pub block_new_entries_when_position_open: bool,
    #[serde(default = "default_max_signal_age")]
    pub max_signal_age_seconds: u64,
    #[serde(default = "default_max_pending_bars")]
    pub max_pending_bars: usize,
    #[serde(default = "default_automation_poll")]
    pub automation_poll_seconds: u64,
    #[serde(default = "default_session_preset")]
    pub automation_session_preset: String,
    #[serde(default = "default_session_timezone")]
    pub automation_session_timezone: String,
    #[serde(default = "default_session_start")]
    pub automation_session_start: String,
    #[serde(default = "default_session_end")]
    pub automation_session_end: String,
    #[serde(default = "default_session_weekdays")]
    pub automation_session_weekdays: Vec<u32>,
}

fn default_magic_number() -> i64 { 20260907 }
fn default_command_timeout() -> u64 { 20 }
fn default_bridge_stale() -> u64 { 15 }
fn default_order_size() -> f64 { 0.1 }
fn default_leverage() -> f64 { 100.0 }
fn default_risk_percent() -> f64 { 2.0 }
fn default_max_margin_percent() -> f64 { 25.0 }
fn default_max_signal_age() -> u64 { 120 }
fn default_max_pending_bars() -> usize { 3 }
fn default_automation_poll() -> u64 { 20 }
fn default_session_preset() -> String { "always".to_string() }
fn default_session_timezone() -> String { "UTC".to_string() }
fn default_session_start() -> String { "00:00".to_string() }
fn default_session_end() -> String { "00:00".to_string() }
fn default_session_weekdays() -> Vec<u32> { vec![0, 1, 2, 3, 4, 5, 6] }

impl Default for MT5Settings {
    fn default() -> Self {
        Self {
            bridge_token: String::new(),
            magic_number: default_magic_number(),
            command_timeout_seconds: default_command_timeout(),
            bridge_stale_seconds: default_bridge_stale(),
            auto_trading_enabled: false,
            live_trading_acknowledged: false,
            default_order_size: default_order_size(),
            default_leverage: default_leverage(),
            auto_order_sizing: true,
            risk_percent: default_risk_percent(),
            max_margin_percent: default_max_margin_percent(),
            block_new_entries_when_position_open: true,
            max_signal_age_seconds: default_max_signal_age(),
            max_pending_bars: default_max_pending_bars(),
            automation_poll_seconds: default_automation_poll(),
            automation_session_preset: default_session_preset(),
            automation_session_timezone: default_session_timezone(),
            automation_session_start: default_session_start(),
            automation_session_end: default_session_end(),
            automation_session_weekdays: default_session_weekdays(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub provider: AIProviderSettings,
    #[serde(default)]
    pub general: GeneralSettings,
    #[serde(default)]
    pub prompt: PromptSettings,
    #[serde(default)]
    pub validation: ValidationSettings,
    #[serde(default)]
    pub mt5: MT5Settings,
}

/// 启动时由 .env 文件注入进程环境的键（dotenvy 语义下"文件所有"的键）。
/// 热加载时这些键以新 .env 文件内容为准，进程环境中遗留的陈旧注入值作废；
/// 不在此集合中的键视为显式环境变量（如 Docker compose 的 environment:），
/// 仍然优先于 .env 文件值——保持容器部署语义不变。
static FILE_OWNED_ENV_KEYS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn file_owned_env_keys() -> &'static Mutex<HashSet<String>> {
    FILE_OWNED_ENV_KEYS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// 读取当前工作目录下的 .env 文件为键值对（与 dotenvy 的查找路径一致）。
fn read_env_file_map() -> HashMap<String, String> {
    parse_env_file(Path::new(".env"))
}

/// 简单解析 .env 文件：跳过注释与空行，支持 export 前缀，去除成对引号；
/// 值中的 `=` 与 `#` 原样保留（兼容令牌/URL 等特殊字符）。
fn parse_env_file(path: &Path) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Ok(content) = std::fs::read_to_string(path) else {
        return map;
    };
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim();
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let key = k.trim().to_string();
        if key.is_empty() {
            continue;
        }
        let mut value = v.trim().to_string();
        if value.len() >= 2 {
            let bytes = value.as_bytes();
            let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
            if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
                value = value[1..value.len() - 1].to_string();
            }
        }
        map.insert(key, value);
    }
    map
}

fn load_settings_json<P: AsRef<Path>>(path: P) -> Settings {
    if path.as_ref().exists() {
        match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str::<Settings>(&content).unwrap_or_default(),
            Err(_) => Settings::default(),
        }
    } else {
        Settings::default()
    }
}

/// 按给定读取源应用环境变量覆盖。`get` 返回 None 表示该键不存在；
/// 值为空串时按原语义跳过（布尔/数字项由各自的解析规则处理）。
fn apply_env_overrides(settings: &mut Settings, get: impl Fn(&str) -> Option<String>) {
    let val = |names: &[&str]| -> Option<String> { names.iter().find_map(|n| get(n)) };

    if let Some(v) = val(&["LLM_API_KEY", "AI_API_KEY"]) {
        if !v.trim().is_empty() { settings.provider.api_key = v.trim().to_string(); }
    }
    if let Some(v) = val(&["LLM_BASE_URL", "AI_BASE_URL"]) {
        if !v.trim().is_empty() { settings.provider.base_url = v.trim().to_string(); }
    }
    if let Some(v) = val(&["LLM_MODEL", "AI_MODEL"]) {
        if !v.trim().is_empty() { settings.provider.model = v.trim().to_string(); }
    }
    if let Some(v) = val(&["LLM_THINKING", "AI_THINKING"]) {
        settings.provider.thinking = v.trim().eq_ignore_ascii_case("true") || v.trim() == "1";
    }
    if let Some(v) = val(&["LLM_REASONING_EFFORT", "AI_REASONING_EFFORT"]) {
        if !v.trim().is_empty() { settings.provider.reasoning_effort = v.trim().to_string(); }
    }
    if let Some(v) = val(&["LLM_CONTEXT_WINDOW", "AI_CONTEXT_WINDOW"]) {
        if let Ok(num) = v.trim().parse::<usize>() { settings.provider.context_window = num; }
    }
    if let Some(v) = val(&["LLM_STAGE_TIMEOUT_SECONDS", "AI_STAGE_TIMEOUT_SECONDS"]) {
        if let Ok(num) = v.trim().parse::<u64>() { settings.provider.stage_timeout_seconds = num; }
    }
    if let Some(v) = val(&["TRADING_SYSTEM", "AI_TRADING_SYSTEM"]) {
        if !v.trim().is_empty() { settings.general.trading_system = v.trim().to_string(); }
    }

    if let Some(v) = val(&["MT5_BRIDGE_TOKEN"]) {
        settings.mt5.bridge_token = v.trim().to_string();
    }
    if let Some(v) = val(&["MT5_MAGIC_NUMBER"]) {
        if let Ok(num) = v.trim().parse::<i64>() { settings.mt5.magic_number = num; }
    }
    if let Some(v) = val(&["MT5_COMMAND_TIMEOUT_SECONDS"]) {
        if let Ok(num) = v.trim().parse::<u64>() { settings.mt5.command_timeout_seconds = num; }
    }
    if let Some(v) = val(&["MT5_BRIDGE_STALE_SECONDS"]) {
        if let Ok(num) = v.trim().parse::<u64>() { settings.mt5.bridge_stale_seconds = num; }
    }
    if let Some(v) = val(&["MT5_AUTO_TRADING_ENABLED"]) {
        settings.mt5.auto_trading_enabled = v.trim().eq_ignore_ascii_case("true") || v.trim() == "1";
    }
    if let Some(v) = val(&["MT5_LIVE_TRADING_ACKNOWLEDGED"]) {
        settings.mt5.live_trading_acknowledged = v.trim().eq_ignore_ascii_case("true") || v.trim() == "1";
    }
    if let Some(v) = val(&["MT5_DEFAULT_ORDER_SIZE"]) {
        if let Ok(num) = v.trim().parse::<f64>() { settings.mt5.default_order_size = num; }
    }
    if let Some(v) = val(&["MT5_AUTO_ORDER_SIZING"]) {
        settings.mt5.auto_order_sizing = v.trim().eq_ignore_ascii_case("true") || v.trim() == "1";
    }
    if let Some(v) = val(&["MT5_RISK_PERCENT"]) {
        if let Ok(num) = v.trim().parse::<f64>() { settings.mt5.risk_percent = num.clamp(0.1, 20.0); }
    }
    if let Some(v) = val(&["MT5_MAX_MARGIN_PERCENT"]) {
        if let Ok(num) = v.trim().parse::<f64>() { settings.mt5.max_margin_percent = num.clamp(1.0, 100.0); }
    }
    if let Some(v) = val(&["MT5_DEFAULT_LEVERAGE"]) {
        if let Ok(num) = v.trim().parse::<f64>() { settings.mt5.default_leverage = num; }
    }
    if let Some(v) = val(&["MT5_BLOCK_NEW_ENTRIES_WHEN_POSITION_OPEN"]) {
        settings.mt5.block_new_entries_when_position_open = v.trim().eq_ignore_ascii_case("true") || v.trim() == "1";
    }
    if let Some(v) = val(&["MT5_MAX_SIGNAL_AGE_SECONDS"]) {
        if let Ok(num) = v.trim().parse::<u64>() { settings.mt5.max_signal_age_seconds = num; }
    }
    if let Some(v) = val(&["MT5_MAX_PENDING_BARS"]) {
        if let Ok(num) = v.trim().parse::<usize>() { settings.mt5.max_pending_bars = num; }
    }
    if let Some(v) = val(&["MT5_AUTOMATION_SESSION_PRESET"]) {
        if !v.trim().is_empty() { settings.mt5.automation_session_preset = v.trim().to_string(); }
    }
    if let Some(v) = val(&["MT5_AUTOMATION_SESSION_TIMEZONE"]) {
        if !v.trim().is_empty() { settings.mt5.automation_session_timezone = v.trim().to_string(); }
    }
}

impl Settings {
    /// 启动时加载：.env 注入进程环境变量（dotenvy 语义：已存在的不覆盖），
    /// 再叠加 settings.json 与环境变量覆盖。
    pub fn load_from_file_and_env<P: AsRef<Path>>(path: P) -> Self {
        // 在 dotenvy 注入前记录键归属：进程环境中尚不存在的键 = 由文件注入
        {
            let mut owned = file_owned_env_keys().lock().unwrap();
            for key in read_env_file_map().keys() {
                if std::env::var(key).is_err() {
                    owned.insert(key.clone());
                }
            }
        }
        let _ = dotenvy::dotenv();

        let mut settings = load_settings_json(path);
        apply_env_overrides(&mut settings, |k| std::env::var(k).ok());
        settings
    }

    /// 热加载（保存配置后调用）：绕过进程环境变量，直接以新 .env 文件内容为准。
    ///
    /// 背景：进程环境变量里遗留着启动时 dotenvy 注入的旧 .env 值，且 dotenvy
    /// 不会覆盖已存在的变量——若热加载走 env::var 路径，读到的永远是旧配置。
    /// 归属规则：
    /// - 启动时由 .env 注入的键 → 以新文件值为准（陈旧注入值作废）；
    /// - 显式环境变量（Docker environment: 等）→ 优先于文件值。
    pub fn load_for_hot_reload<P: AsRef<Path>>(path: P) -> Self {
        let file_map = read_env_file_map();
        let owned = file_owned_env_keys().lock().unwrap().clone();

        let mut settings = load_settings_json(path);
        apply_env_overrides(&mut settings, |k| {
            if owned.contains(k) {
                file_map.get(k).cloned()
            } else {
                std::env::var(k).ok().or_else(|| file_map.get(k).cloned())
            }
        });
        settings
    }

    pub fn is_provider_configured(&self) -> bool {
        !self.provider.api_key.trim().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_env_file() {
        let dir = std::env::temp_dir().join(format!("wb_env_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".env");
        std::fs::write(
            &path,
            "# 注释行\n\
             \n\
             LLM_API_KEY=sk-abc123\n\
             export LLM_MODEL=\"test-model\"\n\
             MT5_BRIDGE_TOKEN='tok=en#x'\n\
             LLM_BASE_URL=https://api.example.com/v1?key=1&x=2\n\
             MT5_RISK_PERCENT=2.5\n\
             INVALID_LINE_NO_EQUALS\n",
        )
        .unwrap();

        let m = parse_env_file(&path);
        assert_eq!(m.get("LLM_API_KEY").unwrap(), "sk-abc123");
        assert_eq!(m.get("LLM_MODEL").unwrap(), "test-model"); // export + 双引号
        assert_eq!(m.get("MT5_BRIDGE_TOKEN").unwrap(), "tok=en#x"); // 单引号 + 值含 = 和 #
        assert_eq!(m.get("LLM_BASE_URL").unwrap(), "https://api.example.com/v1?key=1&x=2");
        assert_eq!(m.get("MT5_RISK_PERCENT").unwrap(), "2.5");
        assert!(!m.contains_key("INVALID_LINE_NO_EQUALS"));
        assert_eq!(m.len(), 5);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_apply_env_overrides_from_map() {
        let mut map = HashMap::new();
        map.insert("LLM_API_KEY".to_string(), "sk-hot".to_string());
        map.insert("LLM_MODEL".to_string(), "aio".to_string());
        map.insert("TRADING_SYSTEM".to_string(), "dog_walking".to_string());
        map.insert("MT5_MAGIC_NUMBER".to_string(), "20260907".to_string());
        map.insert("MT5_RISK_PERCENT".to_string(), "5".to_string()); // 触发 clamp 规则
        map.insert("MT5_AUTO_TRADING_ENABLED".to_string(), "true".to_string());
        map.insert("LLM_API_KEY_FALLBACK_UNUSED".to_string(), "x".to_string());

        let mut s = Settings::default();
        apply_env_overrides(&mut s, |k| map.get(k).cloned());
        assert_eq!(s.provider.api_key, "sk-hot");
        assert_eq!(s.provider.model, "aio");
        assert_eq!(s.general.trading_system, "dog_walking");
        assert_eq!(s.mt5.magic_number, 20260907);
        assert!((s.mt5.risk_percent - 5.0).abs() < 1e-9);
        assert!(s.mt5.auto_trading_enabled);
        assert!(s.is_provider_configured());
    }

    #[test]
    fn test_apply_env_overrides_empty_value_keeps_default() {
        let mut map = HashMap::new();
        map.insert("LLM_API_KEY".to_string(), "  ".to_string()); // 空白值应跳过
        let mut s = Settings::default();
        apply_env_overrides(&mut s, |k| map.get(k).cloned());
        assert!(s.provider.api_key.is_empty());
        assert!(!s.is_provider_configured());
    }

    #[test]
    fn test_hot_reload_getter_semantics() {
        // 场景：启动时 .env 注入的键（owned）→ 以新文件值为准，即使进程环境仍留着旧值；
        // 显式环境变量（非 owned）→ 优先于文件值。
        // 用独立测试键名避免与其他并行测试的进程环境冲突。
        std::env::set_var("WB_TEST_EXPLICIT_MODEL", "explicit-model");

        let mut file_map = HashMap::new();
        file_map.insert("WB_TEST_EXPLICIT_MODEL".to_string(), "file-model".to_string());
        file_map.insert("LLM_API_KEY".to_string(), "new-key".to_string());

        let mut owned = HashSet::new();
        owned.insert("LLM_API_KEY".to_string()); // 文件注入的键
        // WB_TEST_EXPLICIT_MODEL 不在 owned 中 → 视为显式环境变量

        let getter = |k: &str| {
            if owned.contains(k) {
                file_map.get(k).cloned()
            } else {
                std::env::var(k).ok().or_else(|| file_map.get(k).cloned())
            }
        };

        // owned 键：文件值胜出（不读进程环境）
        assert_eq!(getter("LLM_API_KEY").unwrap(), "new-key");
        // 非 owned 键：显式环境变量胜出
        assert_eq!(getter("WB_TEST_EXPLICIT_MODEL").unwrap(), "explicit-model");
        // 非 owned 且环境缺失：回退文件值
        assert_eq!(getter("LLM_MODEL").is_none(), true);
        std::env::remove_var("WB_TEST_EXPLICIT_MODEL");
    }
}
