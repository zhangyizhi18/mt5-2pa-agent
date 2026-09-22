# MT5 2PA Agent (Rust 高性能版 · MetaTrader 5 桥接版)

![Rust](https://img.shields.io/badge/language-Rust%201.75+-orange.svg)
![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20MetaTrader%205-blue.svg)
![License](https://img.shields.io/badge/License-AGPL--3.0-blue.svg)

**MT5 2PA Agent** 是 [2pa-agent-rust](https://github.com/oficcejo/2pa-agent-rust)（OKX 版）的 **MetaTrader 5 移植版**：保留原项目全部框架（Al Brooks 价格行为学 + 🐕 遛狗均线回归 + LLM 两阶段智能推理 + 内嵌 Web 控制台），把交易所对接层从 OKX REST 替换为 **MetaTrader 5 桥接（PABridge EA）**，用于外汇、贵金属、指数、加密等 MT5 品种的日内量化交易。

```
┌────────────────────────────┐          HTTP 轮询 (~1s)           ┌──────────────────────────┐
│  mt5-2pa-agent (纯 Rust)    │ ◄──── /bridge/sync (T2A 状态) ──── │  MetaTrader 5 终端         │
│  · Web 控制台 (Axum:8066)   │ ───── /bridge/sync (A2T 指令) ───► │  · PABridge EA (MQL5)     │
│  · LLM 两阶段 AI 推理       │                                    │  · CopyRates / CTrade     │
│  · 风控与审计               │                                    │  · 下单/撤单/改SLTP/平仓   │
└────────────────────────────┘                                    └──────────────────────────┘
```

**工作方式**：标准 MQL5 不允许在终端内监听 TCP/HTTP 端口，因此桥接采用"**EA 轮询**"架构 —— `PABridge.mq5` EA 通过 `WebRequest` 每秒向 Rust 服务上报账户、持仓、挂单、品种规格与 K 线，同时取回待执行指令（市价/限价/突破挂单、撤单、修改止盈止损、平仓、订阅品种）并执行。行情延迟约等于轮询间隔，完全满足"新收盘 K 线触发一次分析"的本系统交易节奏。

---

## 🌟 保留的完整功能（框架不变）

- **双 AI 交易系统**：📊 2PA 价格行为系统（Al Brooks 八态周期、EMA20、H2/L2、楔形、突破测试）与 🐕 遛狗系统（SMA 14/170 均线偏离回归、TP2 锚定 170 主人均线），外加 🧠 智能自适应双引擎。
- **两阶段 LLM 推理**：阶段一市场诊断 + 阶段二元决策（JSON 校验与自愈重试），兼容所有 OpenAI 兼容接口（默认 DeepSeek）。
- **撤旧换新 (Cancel-Replace)**：新委托前自动撤销同品种本代理（魔术号匹配）旧挂单，杜绝堆积。
- **结构化止盈止损**：下单即附带 SL/TP（MT5 原生订单属性，持仓不依赖本机）；止损距离超 2×ATR14 或通道 50% 放弃；TP1 盈亏比 ≥1.0；动态算量（风险比例 + 保证金顶格双约束）。
- **AI 主动风控动作**：平仓 (CLOSE_EARLY)、单向移损 (MOVE_STOP_LOSS 拒绝逆向扩损)、动态移盈 (MOVE_TAKE_PROFIT)、同步 SL/TP。
- **Web 控制台**：实时 K 线图（双系统均线切换）、决策面板、账户总览（权益/持仓/挂单/一键全撤）、📐 手数换算器、🤖 自动交易调度与多时段过滤器。
- **经验库与审计**：`experience/` 价格行为经验库、决策记录、`records/trade_audit.jsonl` 交易审计流水。

## 🔄 与 OKX 版的差异

| 项目 | OKX 版 | 本 MT5 版 |
| :--- | :--- | :--- |
| 对接层 | OKX v5 REST + HMAC 签名 | PABridge EA HTTP 轮询（无签名需求，可选共享令牌） |
| 品种 | `BTC-USDT-SWAP` 等 | MT5 品种名：`XAUUSD`、`EURUSD`、`BTCUSD` 等（以终端"市场报价"窗口为准） |
| 数量单位 | 合约张数 / 币数 | **手数 (Lot)** |
| 模拟/实盘 | `.env` 中配置 | 由 MT5 登录账户类型**自动识别**（demo/contest/real） |
| 止盈止损单 | 交易所端 `attachAlgoOrds` | MT5 订单/持仓原生 SL/TP 字段 |
| 修改止盈止损 | 修改 algo 条件单 | 修改持仓 SL/TP (`PositionModify`) |
| 杠杆 | 下单前 set-leverage | 账户级设置（终端内调整），仅用于保证金估算 |
| 部署 | Docker / 多平台 | Rust 服务任意平台；EA 依赖 Windows/macOS 的 MT5 终端 |

> 移植时顺带修复了原版一个安全漏洞：`/static/` 路由的目录穿越（原版可通过 URL 编码读取 `.env`），本版已拦截并更新测试。

---

## 🚀 快速开始

### 1. 编译 Rust 服务

```bash
# 安装 Rust 1.75+ (https://rustup.rs)
cargo build --release
# 产物: target/release/mt5-2pa-agent.exe
```

### 2. 安装 PABridge EA（MT5 终端内）

1. 打开 MT5 → `文件` → `打开数据文件夹` → 进入 `MQL5\Experts\`，把本仓库 `mql5/PABridge.mq5` 复制进去。
2. 在 MetaEditor 中打开该文件并编译（F7），确认 0 errors。
3. **关键一步**：MT5 → `工具` → `选项` → `EA 交易`：
   - 勾选 **"允许 WebRequest 用于列出的 URL"**；
   - 添加 `http://127.0.0.1:8066`（Rust 服务与本机 MT5 同机时的默认地址；跨机则填 Rust 服务所在 IP）。
4. 把 EA 挂到**任意一张图表**（建议 XAUUSD M15），勾选"允许算法交易"。图表左上角出现 `PABridge 运行中 / 状态: 已连接` 即成功。

**EA 输入参数**：

| 参数 | 默认值 | 说明 |
| :--- | :--- | :--- |
| `BridgeUrl` | `http://127.0.0.1:8066/bridge/sync` | Rust 服务同步端点 |
| `BridgeToken` | 空 | 共享令牌，与 `.env` 的 `MT5_BRIDGE_TOKEN` 一致（留空不校验） |
| `PollMs` | `1000` | 轮询间隔（毫秒），≥200 |
| `MagicNumber` | `20260907` | 魔术号，与 `.env` 的 `MT5_MAGIC_NUMBER` **必须一致** |
| `Watchlist` | `XAUUSD` | 初始监控品种（逗号分隔；界面查询其它品种会自动动态订阅） |
| `WatchTfs` | `M15` | 初始监控周期（如 `M5,M15,H1`） |
| `BarsToSync` | `400` | 每个系列同步 K 线数量（分析需要 ≥300） |

### 3. 配置并启动

```bash
copy .env.example .env   # 编辑 LLM_API_KEY 等
start.bat                # 或 cargo run --release
```

浏览器打开 `http://127.0.0.1:8066/`：首次未配置会自动弹出 Web 配置向导（LLM Key、魔术号、桥接令牌、风控参数），保存即写入 `.env` 并热加载。

### 4. 验证桥接

- 顶部徽标显示 **"MT5 桥接已连接"**、账户类型（模拟/实盘）自动识别；
- 决策页选择品种（如 `XAUUSD`）与周期，点击"运行 AI 分析"；
- 开启自动交易：模拟账户输入 `ENABLE DEMO`、实盘输入 `ENABLE LIVE` 确认。

> 💡 **品种名与经纪商后缀**：MT5 品种名区分大小写，部分经纪商使用带后缀的命名（如 Exness 的 `XAUUSDm`、`XAUUSDc`）。EA 会对代理下发的品种名做**大小写不敏感的自动解析**——界面上输入 `XAUUSDM`、`xauusdm` 或 `XAUUSDm` 都能命中，下单时自动使用终端规范名。但请确保 EA 的 `Watchlist` 输入参数与界面查询的品种**确实存在**于当前终端的"市场报价"窗口（品种对不上时，服务会提示"品种暂未在终端找到"）。

---

## 📡 REST API 概览

| 请求方法 | 路由路径 | 说明 |
| :--- | :--- | :--- |
| `GET` | `/` | 渲染 Web 控制台主页 |
| `POST` | `/bridge/sync` | **PABridge EA 专用**：T2A 状态上报 / A2T 指令下发 |
| `GET` | `/api/status` | 系统状态、桥接连接信息、当前交易系统与自动化配置 |
| `GET` | `/api/instruments` | EA 已上报的 MT5 品种列表 |
| `GET` | `/api/candles?inst_id=XAUUSD&timeframe=15m&limit=300` | 已计算指标的 K 线（首次查询会自动向 EA 动态订阅） |
| `GET` | `/api/account` | 账户总览、持仓与挂单 |
| `POST` | `/api/trade/cancel` | 撤销指定挂单（`ticket`） |
| `POST` | `/api/trade/cancel_all` | 一键撤销本代理（魔术号匹配）全部挂单 |
| `POST` | `/api/analyze` | 触发单次两阶段 AI 诊断与决策（可附 `execute=true` 执行） |
| `POST` | `/api/automation` | 开启/关闭后台新 K 线自动交易调度 |
| `POST` | `/api/trading_system` | 切换交易系统 (`2pa` / `dog_walking` / `adaptive`) |
| `GET` | `/api/contract/specs` | 品种规格与 1 手价值换算 |
| `GET/POST` | `/api/config`、`/api/config/save_env` | 配置读取与 `.env` 持久化 |
| `GET/DELETE` | `/api/history/decisions`、`/api/history/trades` | 决策与交易审计历史 |

## 🔌 桥接协议 (v1)

EA ↔ 服务使用极简分行文本协议（纯 ASCII 分隔符，便于 MQL5 解析）：

**EA → 服务（POST /bridge/sync，T2A v1）**

```text
T2A 1
AUTH <token>                     # 配置了 BridgeToken 时必填
TERM name=..|company=..|connected=1|trade_allowed=1|srvtime=1700000000
ACCT login=..|currency=USD|balance=..|equity=..|margin=..|freemargin=..|marginlevel=..|profit=..|leverage=100|trademode=0|marginmode=0|server=..
SYM XAUUSD|desc=Gold|point=0.01|digits=2|tick=0.01|vmin=0.01|vstep=0.01|vmax=100|contract=100|margininit=0|bid=..|ask=..|last=..|trademode=4
POS <ticket>|<symbol>|<0多/1空>|<volume>|<open>|<current>|<sl>|<tp>|<profit>|<swap>|<commission>|<time>|<magic>|<comment>
ORD <ticket>|<symbol>|<buy_limit等>|<volume>|<price>|<sl>|<tp>|<time>|<magic>|<comment>
BAR XAUUSD:M15                   # K 线系列头
BART <time>,<o>,<h>,<l>,<c>,<v>  # 时间升序，最新一根在最后
RES id=..|ok=1|msg=..|data=..    # 指令执行结果回执
END
```

**服务 → EA（响应体，A2T v1）**

```text
A2T 1
CMD id=..|action=MARKET|symbol=XAUUSD|side=buy|volume=0.10|sl=..|tp=..|comment=pa..|magic=..
CMD id=..|action=LIMIT|...|price=..|...          # 限价挂单
CMD id=..|action=STOP|...|price=..|...           # 突破挂单
CMD id=..|action=CANCEL|ticket=..                # 撤销挂单
CMD id=..|action=MODIFY|ticket=..|sl=..|tp=..    # 修改持仓 SL/TP（留空保持不变）
CMD id=..|action=CLOSE|ticket=..                 # 平仓
CMD id=..|action=SUBSCRIBE|series=XAUUSD:M15|bars=400
CMD id=..|action=UNSUBSCRIBE|series=...
CMD id=..|action=SPEC|symbol=XAUUSD
END
```

安全设计：EA 对 `id` 做环形去重（重复指令回放缓存结果，绝不重复执行）；服务端对下单指令统一做信号去重、时效校验、三价关系校验与审计落盘；EA 上报的服务器时间用于 K 线闭合判定，避免时区偏差。

---

## 🧪 自动化测试

```bash
cargo test -- --nocapture
```

覆盖：MT5 协议解析/序列化回环、K 线闭合判定与排序约定、动态算量（风险/保证金双约束）、下单意图构造（限价/突破映射、tick 取整、信心门槛、三价关系）、指标算法（EMA/ATR/SMA）、遛狗系统 Prompt 组装、交易时段过滤器、JSON 校验自愈、静态资源路径穿越防护等 **40 项测试**。

---

## ⚠️ 风险免责声明

1. 本软件仅供量化策略研究与价格行为学教学交流使用，不构成任何投资建议或财务指导。
2. 外汇/贵金属/衍生品交易具有极高的市场风险，可能导致本金全部损失。
3. 请在**模拟账户**中充分测试后再考虑实盘；开启实盘需输入 `ENABLE LIVE` 确认。EA 下单依赖 MT5 终端与网络保持运行，请自行做好风控。开发者不对任何因程序运行或交易决策产生的经济损失承担责任。

## 📄 开源许可证与来源声明

- **上游原项目**：[oficcejo/2pa-agent-rust](https://github.com/oficcejo/2pa-agent-rust)（OKX 2PA Agent），版权归 **okx-2pa-agent-web Contributors** 所有（`Copyright (C) 2026 okx-2pa-agent-web Contributors`）。
- **本项目**：`mt5-2pa-agent` 是上游项目的 **MetaTrader 5 移植版**，保留原项目的框架、提示词工程、指标算法、编排与 Web 控制台，仅把交易所对接层从 OKX v5 REST 替换为 MT5 PABridge EA 轮询。
- **许可证**：沿用上游 [GNU AGPL v3](LICENSE)（`AGPL-3.0-or-later`，SPDX 标识见 `LICENSE` 文件）。这是**强 copyleft** 许可证，并带**网络服务条款**：修改本项目并通过网络对外提供服务的，必须向使用者提供完整对应源码。
- 移植版新增与修改部分的版权归 `mt5-2pa-agent Contributors` 所有，同样以 `AGPL-3.0-or-later` 授权。
- 来源、修改范围与许可证义务的完整说明见 [NOTICE](NOTICE)。
- **保留原版权声明是二次分发的硬性要求**：`LICENSE` 中的 `Copyright (C) 2026 okx-2pa-agent-web Contributors` 与根目录 [NOTICE](NOTICE) 请勿删除或改写。
