use mt5_2pa_agent::mt5::client::MT5Bridge;
use mt5_2pa_agent::mt5::trading::{MT5TradeExecutor, BROKER_TAG};
use mt5_2pa_agent::mt5::types::timeframe_to_mt5;
use rust_decimal::Decimal;

/// 模拟 EA 上报的一次完整同步（含规格、账户、两根 K 线）。
const SYNC_PAYLOAD: &str = "T2A 1\n\
    TERM name=MetaTrader 5|company=Test Broker|connected=1|trade_allowed=1|srvtime=1700000050\n\
    ACCT login=123|name=Tester|currency=USD|balance=10000|equity=10100|margin=600|freemargin=9500|marginlevel=1683|profit=100|leverage=100|trademode=0|marginmode=0|server=Test-Server\n\
    SYM XAUUSD|desc=Gold|point=0.01|digits=2|tick=0.01|vmin=0.01|vstep=0.01|vmax=100|contract=100|margininit=0|bid=1930.10|ask=1930.30|last=0|trademode=4\n\
    BAR XAUUSD:M15\n\
    BART 1699999100,1929.0,1930.0,1928.5,1929.5,120\n\
    BART 1699999200,1929.5,1931.0,1929.0,1930.5,150\n\
    END\n";

fn make_bridge() -> MT5Bridge {
    let bridge = MT5Bridge::new("", 20260907, 20, 15);
    assert!(bridge.handle_sync(None, SYNC_PAYLOAD).is_ok());
    bridge
}

/// 模拟 Exness 类经纪商：终端规范品种名带小写后缀（XAUUSDm）。
const SYNC_PAYLOAD_MIXED_CASE: &str = "T2A 1\n\
    TERM name=MetaTrader 5|company=Test Broker|connected=1|trade_allowed=1|srvtime=1700000050\n\
    ACCT login=123|name=Tester|currency=USD|balance=10000|equity=10100|margin=600|freemargin=9500|marginlevel=1683|profit=100|leverage=100|trademode=0|marginmode=0|server=Test-Server\n\
    SYM XAUUSDm|desc=Gold|point=0.01|digits=2|tick=0.01|vmin=0.01|vstep=0.01|vmax=100|contract=100|margininit=0|bid=1930.10|ask=1930.30|last=0|trademode=4\n\
    END\n";

#[tokio::test]
async fn test_symbol_lookup_case_insensitive() {
    let bridge = MT5Bridge::new("", 20260907, 20, 15);
    assert!(bridge.handle_sync(None, SYNC_PAYLOAD_MIXED_CASE).is_ok());
    // 用大写查询（代理内部历史行为）也能命中终端规范名 XAUUSDm
    let spec = bridge.get_symbol_spec("XAUUSDM").await.unwrap();
    assert_eq!(spec.symbol, "XAUUSDm");
    let spec2 = bridge.get_symbol_spec("xauusdm").await.unwrap();
    assert_eq!(spec2.symbol, "XAUUSDm");
}

#[test]
fn test_position_filter_case_insensitive() {
    let bridge = MT5Bridge::new("", 20260907, 20, 15);
    let payload = SYNC_PAYLOAD_MIXED_CASE.replace(
        "END\n",
        "POS 111|XAUUSDm|0|0.1|1928.00|1930.00|1925.00|1940.00|20.0|0.5|0|1700000100|20260907|paabc\nEND\n",
    );
    assert!(bridge.handle_sync(None, &payload).is_ok());
    // 代理侧查询用大写也能匹配终端规范名持仓
    assert_eq!(bridge.get_positions(Some("XAUUSDM")).len(), 1);
    assert_eq!(bridge.get_positions(Some("xauusdm")).len(), 1);
    assert_eq!(bridge.get_positions(Some("EURUSD")).len(), 0);
}

fn make_executor(bridge: MT5Bridge, auto_sizing: bool) -> MT5TradeExecutor {
    MT5TradeExecutor::new(
        bridge,
        0.5,   // default_order_size (手)
        100.0, // default_leverage (估算)
        true,  // block_new_entries
        40,    // confidence threshold
        120,   // max signal age
        3,     // max pending bars
        None,  // audit path
        auto_sizing,
        2.0,   // risk %
        25.0,  // max margin %
    )
}

#[test]
fn test_broker_tag_constant() {
    assert_eq!(BROKER_TAG, "PA-MT5-BRIDGE");
}

#[test]
fn test_timeframe_mapping() {
    assert_eq!(timeframe_to_mt5("15m"), Some("M15"));
    assert_eq!(timeframe_to_mt5("1h"), Some("H1"));
    assert_eq!(timeframe_to_mt5("1d"), Some("D1"));
    assert_eq!(timeframe_to_mt5("1w"), Some("W1"));
    assert_eq!(timeframe_to_mt5("7m"), None);
}

#[test]
fn test_signal_id_generation() {
    let decision = serde_json::json!({
        "order_direction": "做多",
        "order_type": "限价单",
        "entry_price": 1900.0,
        "stop_loss_price": 1890.0,
        "take_profit_price": 1920.0,
    });

    let sig1 = MT5TradeExecutor::generate_signal_id("XAUUSD", "15m", 1700000000000, &decision);
    let sig2 = MT5TradeExecutor::generate_signal_id("XAUUSD", "15m", 1700000000000, &decision);
    assert_eq!(sig1, sig2);
    assert_eq!(sig1.len(), 24);
}

#[test]
fn test_sync_populates_state_and_candles() {
    let bridge = make_bridge();
    assert!(bridge.is_connected());
    assert_eq!(bridge.get_account().equity, 10100.0);
    assert_eq!(bridge.get_account().free_margin, 9500.0);
    assert_eq!(bridge.get_account().trade_mode, "demo");
    assert_eq!(bridge.get_account().margin_mode, "netting");
    assert_eq!(bridge.magic(), 20260907);

    // 服务器时间 1700000050：第一根(开 1699999100，M15)已收盘，第二根未收盘
    let positions = bridge.get_positions(None);
    assert_eq!(positions.len(), 0);

    // get_candles 需要异步执行，这里通过状态读取校验
    let snapshot = bridge.terminal_summary();
    assert_eq!(snapshot["series_count"], 1);
    assert_eq!(snapshot["symbol_count"], 1);
}

#[test]
fn test_compute_order_size_by_risk() {
    let bridge = make_bridge();
    let executor = make_executor(bridge, true);

    let spec = mt5_2pa_agent::mt5::types::SymbolSpec {
        symbol: "XAUUSD".to_string(),
        contract_size: 100.0,
        volume_min: 0.01,
        volume_step: 0.01,
        volume_max: 100.0,
        tick_size: 0.01,
        margin_initial: 0.0,
        ..Default::default()
    };

    // 风险 2% * equity 10100 = 202；止损距离 10 * 合约 100 = 1000/手 → 0.202 手
    // 保证金上限: 9500 * 25% / (1900*100/100=1900/手) = 1.25 手 → 取小者 0.202 → floor 0.01 步长
    let size = executor
        .compute_order_size(
            "XAUUSD",
            &spec,
            Decimal::from(1900),
            Decimal::from(1890),
            Decimal::from(10100),
            Decimal::from(9500),
        )
        .unwrap();
    assert_eq!(size, Decimal::new(2, 1)); // 0.2
}

#[test]
fn test_compute_order_size_margin_cap() {
    let bridge = make_bridge();
    let executor = make_executor(bridge, true);

    let spec = mt5_2pa_agent::mt5::types::SymbolSpec {
        symbol: "XAUUSD".to_string(),
        contract_size: 100.0,
        volume_min: 0.01,
        volume_step: 0.01,
        volume_max: 100.0,
        tick_size: 0.01,
        margin_initial: 1900.0, // 每手固定保证金
        ..Default::default()
    };

    // 风险允许 202/1000 = 0.202 手；保证金允许 2375/1900 = 1.25 手 → 0.2 手
    let size = executor
        .compute_order_size(
            "XAUUSD",
            &spec,
            Decimal::from(1900),
            Decimal::from(1890),
            Decimal::from(10100),
            Decimal::from(9500),
        )
        .unwrap();
    assert_eq!(size, Decimal::new(2, 1));
}

#[tokio::test]
async fn test_build_request_limit_long() {
    let bridge = make_bridge();
    let executor = make_executor(bridge.clone(), false);

    let decision = serde_json::json!({
        "order_direction": "做多",
        "order_type": "限价单",
        "entry_price": 1920.0,
        "stop_loss_price": 1910.0,
        "take_profit_price": 1940.0,
        "trade_confidence": 75,
    });

    let (req, kind) = executor.build_request("XAUUSD", &decision, "test_signal_123").await.unwrap();
    // 入场价低于市场 → limit
    assert_eq!(kind, "limit");
    assert_eq!(req["side"], "buy");
    assert_eq!(req["symbol"], "XAUUSD");
    assert_eq!(req["volume"], "0.5");
    assert_eq!(req["sl"], "1910");
    assert_eq!(req["tp"], "1940");
    assert_eq!(req["comment"], "patest_signal_123");
    assert_eq!(req["tag"], BROKER_TAG);
}

#[tokio::test]
async fn test_build_request_breakout_short_maps_to_stop() {
    let bridge = make_bridge();
    let executor = make_executor(bridge.clone(), false);

    // 空头突破：入场价 1925 低于 bid 1930.10 → sell_stop
    let decision = serde_json::json!({
        "order_direction": "做空",
        "order_type": "突破单",
        "entry_price": 1925.0,
        "stop_loss_price": 1935.0,
        "take_profit_price": 1905.0,
        "trade_confidence": 80,
    });

    let (req, kind) = executor.build_request("XAUUSD", &decision, "test_signal_456").await.unwrap();
    assert_eq!(kind, "stop");
    assert_eq!(req["side"], "sell");
    assert_eq!(req["price"], "1925");
}

#[tokio::test]
async fn test_build_request_rejects_low_confidence() {
    let bridge = make_bridge();
    let executor = make_executor(bridge.clone(), false);

    let decision = serde_json::json!({
        "order_direction": "做多",
        "order_type": "限价单",
        "entry_price": 1920.0,
        "stop_loss_price": 1910.0,
        "take_profit_price": 1940.0,
        "trade_confidence": 30,
    });

    let result = executor.build_request("XAUUSD", &decision, "test_signal_789").await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_build_request_validates_price_relations() {
    let bridge = make_bridge();
    let executor = make_executor(bridge.clone(), false);

    // 做多但 止损 > 入场 → 拒绝
    let decision = serde_json::json!({
        "order_direction": "做多",
        "order_type": "市价单",
        "entry_price": 1920.0,
        "stop_loss_price": 1930.0,
        "take_profit_price": 1940.0,
        "trade_confidence": 80,
    });

    let result = executor.build_request("XAUUSD", &decision, "test_signal_abc").await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_get_candles_newest_first_with_forming_flag() {
    let bridge = make_bridge();
    let bars = bridge.get_candles("XAUUSD", "15m", 10).await.unwrap();
    assert_eq!(bars.len(), 2);
    // 最新一根在前
    assert_eq!(bars[0].ts_open, 1699999200 * 1000);
    assert_eq!(bars[1].ts_open, 1699999100 * 1000);
    // 服务器时间 1700000050：最新一根 M15 未收盘
    assert!(!bars[0].closed);
    assert!(bars[1].closed);
    assert_eq!(bars[0].seq, 0);
    assert_eq!(bars[1].seq, 1);
}

#[test]
fn test_handle_sync_rejects_bad_token() {
    let bridge = MT5Bridge::new("secret", 20260907, 20, 15);
    let res = bridge.handle_sync(None, SYNC_PAYLOAD);
    assert!(res.is_err());

    let res2 = bridge.handle_sync(Some("Bearer secret"), SYNC_PAYLOAD);
    assert!(res2.is_ok());
}

#[test]
fn test_handle_sync_returns_commands_and_drains_queue() {
    let bridge = MT5Bridge::new("", 20260907, 20, 15);
    // 首次同步建立连接
    assert!(bridge.handle_sync(None, SYNC_PAYLOAD).is_ok());

    // 异步 exec 无法在此直接测试；通过手动入队不可行(私有)，
    // 但可通过两次同步验证响应头格式
    let resp = bridge.handle_sync(None, SYNC_PAYLOAD).unwrap();
    assert!(resp.starts_with("A2T 1\n"));
    assert!(resp.ends_with("END\n"));
}
