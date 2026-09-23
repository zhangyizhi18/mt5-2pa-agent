# MT5 2PA Agent — Docker 部署详细指南

> 适用版本：mt5-2pa-agent v0.4.4（端口 8066）
> 读完本篇你可以：把 Rust 交易代理跑进 Docker 容器，并让它与另一台/另一处的 MT5 终端桥接成功。

---

## 一、先搞清楚架构（重要！）

这个系统由**两部分**组成，Docker 只负责其中一部分：

```
┌─────────────────────────────┐          ┌──────────────────────────────┐
│  Windows 机器（必须有）        │          │  Docker 容器（本指南部署的部分）  │
│                             │          │                              │
│  MetaTrader 5 终端           │  每1秒    │  mt5-2pa-agent (Rust)         │
│   └─ PABridge.mq5 (EA) ─────┼──轮询───►│  端口 8066                    │
│      上报账户/K线/持仓        │  HTTP    │  · 调用 LLM 分析行情            │
│      执行下单指令             │ ◄────────│  · 生成下单指令                │
└─────────────────────────────┘  POST     └──────────────────────────────┘
                                 /bridge/sync
```

**关键认知：MT5 终端只能运行在 Windows 上，不能进 Docker。** Docker 容器里跑的是 Rust 代理程序。EA 和容器之间通过 HTTP 通信，所以：

- 容器必须监听 `0.0.0.0`（本项目的 Dockerfile 已默认如此，无需修改）
- EA 的 `BridgeUrl` 要填 **Docker 宿主机的 IP**，而不是 127.0.0.1（除非 EA 和容器在同一台机器）

---

## 二、部署前的准备清单

| 项目 | 要求 |
|---|---|
| Docker 宿主机 | Linux 服务器 / Windows / macOS 均可，装有 Docker（20.10+） |
| MT5 端 | 一台 Windows 机器，安装 MT5 终端，能编译运行 PABridge.mq5 |
| 网络 | MT5 机器能访问 Docker 宿主机的 8066 端口 |
| LLM 密钥 | DeepSeek 或其他 OpenAI 兼容接口的 API Key |
| 项目文件 | `2pa-agent-rust-mt5/` 整个文件夹（含 Dockerfile、docker-compose.yml） |

> 如果 EA 和容器跑在同一台 Windows 机器上（Docker Desktop），EA 的 BridgeUrl 填 `http://host.docker.internal` 是**不对的**——那是容器访问宿主机用的域名；EA 在宿主机上，访问容器直接用 `http://127.0.0.1:8066` 即可（因为 compose 已做了端口映射）。

---

## 三、部署步骤（在 Docker 宿主机上执行）

### 步骤 1：把项目传到服务器

把 `2pa-agent-rust-mt5/` 文件夹上传到服务器，例如放到 `/opt/mt5-agent/`：

```bash
# 方式一：scp 上传（在本机执行）
scp -r 2pa-agent-rust-mt5 user@服务器IP:/opt/mt5-agent

# 方式二：如果项目在 Git 仓库，直接克隆
git clone <你的仓库地址> /opt/mt5-agent
```

> 注意：`target/`（编译产物，几个 GB）不需要上传，Docker 构建在容器内完成。`.dockerignore` 已排除它。

### 步骤 2：创建 `.env` 配置文件

```bash
cd /opt/mt5-agent
cp .env.example .env
nano .env   # 或 vim / vi
```

**必须填写的项：**

```ini
# LLM 密钥（必填，否则服务启动后无法分析）
LLM_API_KEY=sk-xxxxxxxxxxxxxxxx

# MT5 桥接令牌：建议设置（桥接流量走局域网/公网，无签名保护，token 是唯一防线）
MT5_BRIDGE_TOKEN=一个随机长字符串
```

**建议确认的项：**

```ini
MT5_MAGIC_NUMBER=20260907        # 要与 EA 输入参数 MagicNumber 一致
MT5_AUTO_TRADING_ENABLED=false   # 首次部署保持 false，验证一切正常后再开
```

> `.env` 含密钥，确认权限：`chmod 600 .env`。它已被 `.gitignore` 和 `.dockerignore` 排除，不会进入镜像和仓库。

### 步骤 3：构建镜像

```bash
cd /opt/mt5-agent
docker compose build
```

首次构建约 5~15 分钟（下载 Rust 工具链 + 编译全部依赖）。构建有层缓存，改代码后重新构建只需重编译改动部分。

看到 `Successfully built` / `naming to ... mt5-2pa-agent:latest` 即成功。

> **国内网络提示**：Dockerfile 默认已把依赖源指向国内镜像（rsproxy），正常情况无需干预。若日志出现
> `unable to update registry crates-io` / `transfer too slow: failed to transfer more than 10 bytes in 30s`，
> 说明镜像源不可用，可换源重建：
> ```bash
> docker compose build --build-arg CARGO_MIRROR=sparse+https://mirrors.ustc.edu.cn/crates.io-index/
> ```
> 海外服务器直连官方源则传空值关闭镜像：
> ```bash
> docker compose build --build-arg CARGO_MIRROR=
> ```

### 步骤 4：启动容器

```bash
docker compose up -d
```

### 步骤 5：验证服务已运行

```bash
# 看容器状态
docker compose ps        # STATUS 应为 Up

# 看启动日志
docker compose logs -f
# 正常日志应包含：监听 0.0.0.0:8066、配置加载成功等字样
# 按 Ctrl+C 退出日志跟踪（容器继续运行）
```

浏览器打开 `http://服务器IP:8066`，能看到 Web 控制台即部署成功。

> 用了 `MT5_BRIDGE_TOKEN` 的话，页面首次打开会进入配置向导，按提示确认即可。

---

## 四、MT5 端配置（在 Windows 机器上）

### 步骤 6：安装 EA

1. 把项目里 `mql5/PABridge.mq5` 复制到 MT5 的数据目录：
   MT5 菜单 → 文件 → 打开数据文件夹 → `MQL5/Experts/`，粘贴进去
2. 在 MetaEditor（按 F4）中打开它，按 F7 编译（应显示 0 errors）
3. 回到 MT5，把 EA 拖到一张图表上（任意品种的图表均可）

### 步骤 7：允许 WebRequest（必做）

EA 要访问 Rust 代理，MT5 默认禁止网络请求，必须加白名单：

MT5 → 工具 → 选项 → EA 交易 → 勾选"允许 WebRequest" → 在列表中添加：

```
http://服务器IP:8066
```

（填你实际的 Docker 宿主机 IP，EA 与容器同机则填 `http://127.0.0.1:8066`）

### 步骤 8：设置 EA 输入参数

挂载 EA 时在"输入"标签页设置：

| 参数 | 填什么 | 说明 |
|---|---|---|
| `BridgeUrl` | `http://服务器IP:8066/bridge/sync` | 指向 Docker 宿主机 |
| `BridgeToken` | 与 `.env` 中 `MT5_BRIDGE_TOKEN` **完全一致** | 不设 token 则两边都留空 |
| `MagicNumber` | `20260907` | 与 `.env` 中 `MT5_MAGIC_NUMBER` 一致 |

点击"确定"，EA 开始每秒轮询。

### 步骤 9：确认桥接已连接

回到浏览器 Web 控制台：

- 顶部桥接状态变为**已连接 / Online**（绿色）
- 能看到账户余额、持仓列表
- 品种列表里能看到 XAUUSD 等已订阅品种

也可以在 MT5 图表左上角看 EA 的 Comment 状态文字（显示轮询状态）。

> ⚠️ **安全提示**：桥接协议本身无签名，token 是唯一凭证。若容器部署在公网服务器，强烈建议：① 设置强随机 token；② 用防火墙/安全组限制 8066 端口来源 IP 只允许 MT5 机器访问；③ 或改用 SSH 隧道/WireGuard 内网互通。

---

## 五、开启自动交易（最后一步）

1. 先在 Web 控制台手动点几次"分析"，确认 LLM 能正常返回、风控校验通过
2. 确认账户类型识别正确（模拟/实盘由 MT5 账户自动识别）
3. 在控制台开启自动交易开关，按提示回复 `ENABLE DEMO`（模拟）或 `ENABLE LIVE`（实盘）确认
4. 建议先在模拟账户（MT5 登录 Demo 账户）跑几天观察

---

## 六、日常运维命令

```bash
docker compose ps              # 查看运行状态
docker compose logs -f         # 实时日志（Ctrl+C 退出）
docker compose logs --tail 200 # 最近 200 行
docker compose restart         # 重启（改了 .env 后需要）
docker compose down            # 停止并删除容器（数据卷不受影响）
docker compose up -d --build   # 改代码后重建并启动
```

**数据持久化说明**（compose 已配置好，无需操作）：

| 挂载 | 内容 |
|---|---|
| `./.env` | 密钥配置（改完记得 restart） |
| `./config` | settings.json |
| `./records` | 决策历史 + `trade_audit.jsonl` 交易审计（删容器不丢失） |

---

## 七、常见问题排查

**Q1：控制台一直显示"桥接未连接"**

按顺序检查：
1. 容器在跑吗？→ `docker compose ps`
2. 服务器防火墙/云安全组放行 8066 了吗？→ `curl http://127.0.0.1:8066` 在服务器本机测，再从 MT5 机器 `curl http://服务器IP:8066` 测
3. MT5 的 WebRequest 白名单加了完整 URL 吗？（必须含 `http://` 前缀）
4. EA 的 BridgeUrl 拼写对吗？（结尾是 `/bridge/sync`）
5. 两边 BridgeToken 是否一致？（一边设了一边没设会 401）

**Q2：日志出现 `MT5_*` 相关配置错误**

环境变量会覆盖 `.env` 文件值，检查 `docker-compose.yml` 的 `environment:` 段是否有冲突项。

**Q3：EA 上报正常但下单失败**

1. 图表查看 EA 的 Comment 是否报错
2. 查审计日志：`cat records/trade_audit.jsonl | tail`，失败原因会记录在案
3. 常见原因：品种后缀不匹配（如 Exness 的 `XAUUSDm`，程序已支持大小写不敏感解析，但需确认 EA 已 SUBSCRIBE 该品种）、magic number 不一致、token 不对

**Q4：容器时间不对导致 K 线判断异常**

容器内用 MT5 服务器时间判断 K 线闭合（时区安全），TZ 环境变量只影响日志显示。确认 compose 里 `TZ=Asia/Shanghai` 即可。

**Q5：重新构建很慢 / 缓存失效**

确认没有修改 `Cargo.toml`/`Cargo.lock`——依赖层缓存只有在这两个文件变化时才会重建。

---

## 附：一键部署命令汇总（老手版）

```bash
cd /opt/mt5-agent
cp .env.example .env && chmod 600 .env
vim .env                                   # 填 LLM_API_KEY 和 MT5_BRIDGE_TOKEN
docker compose build
docker compose up -d
docker compose logs -f
# 浏览器打开 http://服务器IP:8066，然后在 MT5 端配置 PABridge EA
```
