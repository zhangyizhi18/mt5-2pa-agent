//+------------------------------------------------------------------+
//|                                                      PABridge.mq5 |
//|         MT5 2PA Agent 桥接 EA (Price Action Agent Bridge)          |
//|                                                                    |
//| 作用：                                                              |
//|   以固定间隔轮询 Rust 侧服务 (默认 http://127.0.0.1:8066/bridge/sync)，|
//|   上报账户/持仓/挂单/品种规格/K线，并取回并执行交易指令。               |
//|                                                                    |
//| 安装步骤：                                                          |
//|   1. 将本文件复制到 MT5 数据目录 MQL5\Experts\ 下，编译 (F7)。        |
//|   2. 工具 -> 选项 -> EA交易: 勾选"允许 WebRequest 链接"，             |
//|      并把 http://127.0.0.1:8066 加入允许列表。                       |
//|   3. 将 EA 挂载到任意一张图表，确认"允许算法交易"。                    |
//|   4. BridgeToken / MagicNumber 需与 Rust 侧 .env 配置一致。          |
//+------------------------------------------------------------------+
#property copyright "MT5 2PA Agent Contributors"
#property version   "1.00"

#include <Trade/Trade.mqh>

//--- 用户输入参数
input string BridgeUrl    = "http://127.0.0.1:8066/bridge/sync"; // Rust 服务同步地址
input string BridgeToken  = "";                                   // 桥接令牌(与 .env MT5_BRIDGE_TOKEN 一致)
input int    PollMs       = 1000;                                 // 轮询间隔(毫秒)
input long   MagicNumber  = 20260907;                             // 魔术号(与 .env MT5_MAGIC_NUMBER 一致)
input string Watchlist    = "XAUUSD";                             // 初始监控品种(逗号分隔)
input string WatchTfs     = "M15";                                // 初始监控周期(逗号分隔: M5,M15,H1)
input int    BarsToSync   = 400;                                  // 每个系列同步K线数(<=1000)
input int    WebTimeoutMs = 4000;                                 // WebRequest 超时(毫秒)
input bool   VerboseLog   = false;                                // 详细日志

//--- 全局状态
CTrade   trade;
string   g_seriesKeys[];      // "SYM:TF" (与代理请求保持一致)
string   g_seriesSymbols[];   // 解析后的终端规范品种名(可为空=待解析)
ENUM_TIMEFRAMES g_seriesTf[];
int      g_seriesBars[];
string   g_specSymbols[];     // 需要上报规格的品种(存储请求名,上报时解析)
string   g_resReq[];          // 品种解析缓存: 请求名
string   g_resSym[];          // 品种解析缓存: 规范名
string   g_resQueue[];        // 待上报的执行结果 "id|ok|msg|data"
string   g_doneIds[];         // 已执行指令ID环形缓冲(防重复执行)
string   g_doneResults[];     // 与 g_doneIds 对应的结果
int      g_doneHead = 0;
const int DONE_RING = 64;

bool     g_busy = false;
datetime g_lastSyncTime = 0;
string   g_lastError = "";
int      g_syncCount = 0;
bool     g_urlWarned = false;

//+------------------------------------------------------------------+
//| 工具: 字符串清洗                                                   |
//+------------------------------------------------------------------+
string San(const string s)
  {
   string out = s;
   StringReplace(out, "|", "/");
   StringReplace(out, "=", ":");
   StringReplace(out, "\n", " ");
   StringReplace(out, "\r", " ");
   StringReplace(out, ",", " ");
   return out;
  }

//+------------------------------------------------------------------+
//| 工具: 通用键值查找 (k=v|k=v 形式)                                    |
//+------------------------------------------------------------------+
string KvGet(const string kv, const string key)
  {
   string parts[];
   int n = StringSplit(kv, '|', parts);
   for(int i = 0; i < n; i++)
     {
      string pair[];
      if(StringSplit(parts[i], '=', pair) >= 2)
        {
         if(pair[0] == key)
           {
            string val = pair[1];
            for(int j = 2; j < ArraySize(pair); j++)
               val += "=" + pair[j];
            return val;
           }
        }
     }
   return "";
  }

//+------------------------------------------------------------------+
//| 初始化                                                             |
//+------------------------------------------------------------------+
int OnInit()
  {
   trade.SetExpertMagicNumber(MagicNumber);
   trade.SetDeviationInPoints(50);
   trade.SetAsyncMode(false);

   ArrayResize(g_doneIds, DONE_RING);
   ArrayResize(g_doneResults, DONE_RING);

   // 订阅输入参数中的品种 x 周期 (品种名大小写不敏感, 自动解析后缀变体)
   string syms[];
   int ns = StringSplit(Watchlist, ',', syms);
   string tfs[];
   int nt = StringSplit(WatchTfs, ',', tfs);
   for(int i = 0; i < ns; i++)
     {
      string sym = syms[i];
      StringTrimLeft(sym);
      StringTrimRight(sym);
      if(sym == "") continue;
      string resolved = ResolveSymbol(sym);
      if(StringLen(resolved) == 0)
         PrintFormat("PABridge: 警告 - 终端未找到品种 %s (将周期性重试; 请确认市场报价窗口中存在该品种)", sym);
      else
         AddSpecSymbol(resolved);
      for(int j = 0; j < nt; j++)
        {
         string tf = tfs[j];
         StringTrimLeft(tf);
         StringTrimRight(tf);
         if(tf == "") continue;
         string key = sym + ":" + tf;
         StringToUpper(key);
         SubscribeSeries(key, resolved, TfTextToEnum(tf), MathMax(50, BarsToSync));
        }
     }

   EventSetMillisecondTimer(MathMax(200, PollMs));
   PrintFormat("PABridge: 已启动 -> %s (magic=%I64d)", BridgeUrl, MagicNumber);
   return(INIT_SUCCEEDED);
  }

//+------------------------------------------------------------------+
//| 卸载                                                               |
//+------------------------------------------------------------------+
void OnDeinit(const int reason)
  {
   EventKillTimer();
   Comment("");
   PrintFormat("PABridge: 已停止 (reason=%d)", reason);
  }

//+------------------------------------------------------------------+
//| 周期文本 -> ENUM_TIMEFRAMES                                         |
//+------------------------------------------------------------------+
ENUM_TIMEFRAMES TfTextToEnum(const string text)
  {
   string t = text;
   StringTrimLeft(t);
   StringTrimRight(t);
   StringToUpper(t);
   if(t == "M1") return PERIOD_M1;
   if(t == "M2") return PERIOD_M2;
   if(t == "M3") return PERIOD_M3;
   if(t == "M4") return PERIOD_M4;
   if(t == "M5") return PERIOD_M5;
   if(t == "M6") return PERIOD_M6;
   if(t == "M10") return PERIOD_M10;
   if(t == "M12") return PERIOD_M12;
   if(t == "M15") return PERIOD_M15;
   if(t == "M20") return PERIOD_M20;
   if(t == "M30") return PERIOD_M30;
   if(t == "H1") return PERIOD_H1;
   if(t == "H2") return PERIOD_H2;
   if(t == "H3") return PERIOD_H3;
   if(t == "H4") return PERIOD_H4;
   if(t == "H6") return PERIOD_H6;
   if(t == "H8") return PERIOD_H8;
   if(t == "H12") return PERIOD_H12;
   if(t == "D1") return PERIOD_D1;
   if(t == "W1") return PERIOD_W1;
   if(t == "MN1") return PERIOD_MN1;
   return PERIOD_M15;
  }

//+------------------------------------------------------------------+
//| 品种解析: 大小写不敏感地匹配终端品种表                                |
//| (MT5 品种名区分大小写, 如 Exness 的 XAUUSDm / XAUUSDc 等后缀品种)     |
//| 成功解析会缓存; 未找到返回 "" (不缓存失败, 允许后续重试)               |
//+------------------------------------------------------------------+
string ResolveSymbol(const string requested)
  {
   if(StringLen(requested) == 0) return "";
   // 精确匹配快速路径
   if(SymbolSelect(requested, true)) return requested;
   // 查缓存
   for(int i = 0; i < ArraySize(g_resReq); i++)
      if(g_resReq[i] == requested) return g_resSym[i];
   // 遍历终端全部品种做大小写不敏感比较
   int total = SymbolsTotal(false);
   for(int i = 0; i < total; i++)
     {
      string name = SymbolName(i, false);
      if(StringCompare(name, requested, false) == 0)
        {
         SymbolSelect(name, true);
         int n = ArraySize(g_resReq);
         ArrayResize(g_resReq, n + 1);
         ArrayResize(g_resSym, n + 1);
         g_resReq[n] = requested;
         g_resSym[n] = name;
         return name;
        }
     }
   return "";
  }

//+------------------------------------------------------------------+
//| 订阅 K 线系列 (key 与代理请求一致; symbol 为解析结果, 可为空待重试)     |
//+------------------------------------------------------------------+
void SubscribeSeries(const string key, const string resolvedSymbol, const ENUM_TIMEFRAMES tf, const int bars)
  {
   if(StringLen(key) == 0 || tf == PERIOD_CURRENT) return;
   for(int i = 0; i < ArraySize(g_seriesKeys); i++)
     {
      if(g_seriesKeys[i] == key)
        {
         if(StringLen(resolvedSymbol) > 0) g_seriesSymbols[i] = resolvedSymbol;
         g_seriesBars[i] = MathMax(g_seriesBars[i], bars);
         return;
        }
     }
   int n = ArraySize(g_seriesKeys);
   ArrayResize(g_seriesKeys, n + 1);
   ArrayResize(g_seriesSymbols, n + 1);
   ArrayResize(g_seriesTf, n + 1);
   ArrayResize(g_seriesBars, n + 1);
   g_seriesKeys[n] = key;
   g_seriesSymbols[n] = resolvedSymbol;
   g_seriesTf[n] = tf;
   g_seriesBars[n] = MathMin(1000, MathMax(50, bars));
   if(VerboseLog)
      PrintFormat("PABridge: 订阅 %s -> %s (%d 根)", key,
                  StringLen(resolvedSymbol) > 0 ? resolvedSymbol : "(待解析)", g_seriesBars[n]);
  }

void UnsubscribeSeries(const string key)
  {
   int n = ArraySize(g_seriesKeys);
   for(int i = 0; i < n; i++)
     {
      if(g_seriesKeys[i] == key)
        {
         for(int j = i; j < n - 1; j++)
           {
            g_seriesKeys[j] = g_seriesKeys[j + 1];
            g_seriesSymbols[j] = g_seriesSymbols[j + 1];
            g_seriesTf[j] = g_seriesTf[j + 1];
            g_seriesBars[j] = g_seriesBars[j + 1];
           }
         ArrayResize(g_seriesKeys, n - 1);
         ArrayResize(g_seriesSymbols, n - 1);
         ArrayResize(g_seriesTf, n - 1);
         ArrayResize(g_seriesBars, n - 1);
         PrintFormat("PABridge: 退订 %s", key);
         return;
        }
     }
  }

string TfEnumToText(const ENUM_TIMEFRAMES tf)
  {
   switch(tf)
     {
      case PERIOD_M1: return "M1";
      case PERIOD_M2: return "M2";
      case PERIOD_M3: return "M3";
      case PERIOD_M4: return "M4";
      case PERIOD_M5: return "M5";
      case PERIOD_M6: return "M6";
      case PERIOD_M10: return "M10";
      case PERIOD_M12: return "M12";
      case PERIOD_M15: return "M15";
      case PERIOD_M20: return "M20";
      case PERIOD_M30: return "M30";
      case PERIOD_H1: return "H1";
      case PERIOD_H2: return "H2";
      case PERIOD_H3: return "H3";
      case PERIOD_H4: return "H4";
      case PERIOD_H6: return "H6";
      case PERIOD_H8: return "H8";
      case PERIOD_H12: return "H12";
      case PERIOD_D1: return "D1";
      case PERIOD_W1: return "W1";
      case PERIOD_MN1: return "MN1";
     }
   return "M15";
  }

//+------------------------------------------------------------------+
//| 品种规格列表管理                                                    |
//+------------------------------------------------------------------+
void AddSpecSymbol(const string symbol)
  {
   if(symbol == "") return;
   for(int i = 0; i < ArraySize(g_specSymbols); i++)
      if(g_specSymbols[i] == symbol) return;
   int n = ArraySize(g_specSymbols);
   if(n >= 300) return;
   ArrayResize(g_specSymbols, n + 1);
   g_specSymbols[n] = symbol;
  }

//+------------------------------------------------------------------+
//| 结果入队                                                           |
//+------------------------------------------------------------------+
void QueueResult(const string id, const bool ok, const string msg, const string data)
  {
   int n = ArraySize(g_resQueue);
   ArrayResize(g_resQueue, n + 1);
   g_resQueue[n] = id + "|" + (ok ? "1" : "0") + "|" + San(msg) + "|" + San(data);
  }

//+------------------------------------------------------------------+
//| 指令去重环形缓冲                                                    |
//+------------------------------------------------------------------+
bool AlreadyDone(const string id, string &cached)
  {
   for(int i = 0; i < DONE_RING; i++)
     {
      if(g_doneIds[i] == id && g_doneIds[i] != "")
        {
         cached = g_doneResults[i];
         return true;
        }
     }
   return false;
  }

void MarkDone(const string id, const string resultLine)
  {
   g_doneIds[g_doneHead] = id;
   g_doneResults[g_doneHead] = resultLine;
   g_doneHead = (g_doneHead + 1) % DONE_RING;
  }

//+------------------------------------------------------------------+
//| 定时器: 执行一次同步                                                |
//+------------------------------------------------------------------+
void OnTimer()
  {
   if(g_busy) return;
   g_busy = true;
   DoSync();
   g_busy = false;
  }

//+------------------------------------------------------------------+
//| 核心: 上报状态并处理指令                                             |
//+------------------------------------------------------------------+
void DoSync()
  {
   string body = BuildState();

   char post[];
   int len = StringLen(body);
   ArrayResize(post, len);
   StringToCharArray(body, post, 0, len, CP_UTF8);
   // StringToCharArray 会在末尾追加终止符，截掉
   ArrayResize(post, len);

   char result[];
   string resultHeaders;
   string headers = "Content-Type: text/plain\r\n";
   if(StringLen(BridgeToken) > 0)
      headers += "Authorization: Bearer " + BridgeToken + "\r\n";

   ResetLastError();
   int status = WebRequest("POST", BridgeUrl, headers, WebTimeoutMs, post, result, resultHeaders);

   if(status == -1)
     {
      int err = GetLastError();
      if(err == 4014 || err == 4060)
        {
         if(!g_urlWarned)
           {
            g_urlWarned = true;
            Alert("PABridge: WebRequest 被拒绝！请在 MT5 工具->选项->EA交易 中勾选\"允许WebRequest\"",
                  " 并添加 URL: ", BridgeUrl);
           }
        }
      g_lastError = StringFormat("HTTP失败 err=%d", err);
      if(VerboseLog) Print("PABridge: WebRequest error ", err);
      return;
     }
   if(status != 200)
     {
      string errBody = CharArrayToString(result, 0, WHOLE_ARRAY, CP_UTF8);
      g_lastError = StringFormat("HTTP %d: %s", status, San(errBody));
      if(VerboseLog) Print("PABridge: HTTP ", status, " -> ", errBody);
      return;
     }

   string response = CharArrayToString(result, 0, WHOLE_ARRAY, CP_UTF8);
   g_lastError = "";
   g_syncCount++;
   g_lastSyncTime = TimeCurrent();

   ProcessCommands(response);
   UpdateComment();
  }

//+------------------------------------------------------------------+
//| 构造上报状态体 (T2A v1)                                             |
//+------------------------------------------------------------------+
string BuildState()
  {
   string body = "T2A 1\n";
   if(StringLen(BridgeToken) > 0)
      body += "AUTH " + BridgeToken + "\n";

   // ---- 终端信息 ----
   body += StringFormat("TERM name=%s|company=%s|connected=%d|trade_allowed=%d|srvtime=%I64d\n",
                        San(TerminalInfoString(TERMINAL_NAME)),
                        San(TerminalInfoString(TERMINAL_COMPANY)),
                        (TerminalInfoInteger(TERMINAL_CONNECTED) ? 1 : 0),
                        (MQLInfoInteger(MQL_TRADE_ALLOWED) ? 1 : 0),
                        (long)TimeCurrent());

   // ---- 账户信息 ----
   long tradeMode = AccountInfoInteger(ACCOUNT_TRADE_MODE);
   long marginMode = AccountInfoInteger(ACCOUNT_MARGIN_MODE);
   body += StringFormat("ACCT login=%I64d|name=%s|currency=%s|balance=%s|equity=%s|margin=%s|freemargin=%s|marginlevel=%s|profit=%s|leverage=%I64d|trademode=%I64d|marginmode=%I64d|server=%s\n",
                        AccountInfoInteger(ACCOUNT_LOGIN),
                        San(AccountInfoString(ACCOUNT_NAME)),
                        San(AccountInfoString(ACCOUNT_CURRENCY)),
                        DoubleToStr8(AccountInfoDouble(ACCOUNT_BALANCE)),
                        DoubleToStr8(AccountInfoDouble(ACCOUNT_EQUITY)),
                        DoubleToStr8(AccountInfoDouble(ACCOUNT_MARGIN)),
                        DoubleToStr8(AccountInfoDouble(ACCOUNT_MARGIN_FREE)),
                        DoubleToStr8(AccountInfoDouble(ACCOUNT_MARGIN_LEVEL)),
                        DoubleToStr8(AccountInfoDouble(ACCOUNT_PROFIT)),
                        AccountInfoInteger(ACCOUNT_LEVERAGE),
                        tradeMode, marginMode,
                        San(AccountInfoString(ACCOUNT_SERVER)));

   // ---- 品种规格 (上报时解析, 兼容延迟出现的品种) ----
   for(int i = 0; i < ArraySize(g_specSymbols); i++)
     {
      string sym = ResolveSymbol(g_specSymbols[i]);
      if(sym == "") continue;
      if(!SymbolSelect(sym, true)) continue;
      long digits = SymbolInfoInteger(sym, SYMBOL_DIGITS);
      long tradeModeSym = SymbolInfoInteger(sym, SYMBOL_TRADE_MODE);
      body += StringFormat("SYM %s|desc=%s|point=%s|digits=%I64d|tick=%s|vmin=%s|vstep=%s|vmax=%s|contract=%s|margininit=%s|bid=%s|ask=%s|last=%s|trademode=%I64d\n",
                           sym,
                           San(SymbolInfoString(sym, SYMBOL_DESCRIPTION)),
                           DoubleToStr8(SymbolInfoDouble(sym, SYMBOL_POINT)),
                           digits,
                           DoubleToStr8(SymbolInfoDouble(sym, SYMBOL_TRADE_TICK_SIZE)),
                           DoubleToStr8(SymbolInfoDouble(sym, SYMBOL_VOLUME_MIN)),
                           DoubleToStr8(SymbolInfoDouble(sym, SYMBOL_VOLUME_STEP)),
                           DoubleToStr8(SymbolInfoDouble(sym, SYMBOL_VOLUME_MAX)),
                           DoubleToStr8(SymbolInfoDouble(sym, SYMBOL_TRADE_CONTRACT_SIZE)),
                           DoubleToStr8(SymbolInfoDouble(sym, SYMBOL_MARGIN_INITIAL)),
                           DoubleToStr8(SymbolInfoDouble(sym, SYMBOL_BID)),
                           DoubleToStr8(SymbolInfoDouble(sym, SYMBOL_ASK)),
                           DoubleToStr8(SymbolInfoDouble(sym, SYMBOL_LAST)),
                           tradeModeSym);
     }

   // ---- 持仓 ----
   int totalPos = PositionsTotal();
   for(int i = 0; i < totalPos; i++)
     {
      ulong ticket = PositionGetTicket(i);
      if(ticket == 0) continue;
      if(!PositionSelectByTicket(ticket)) continue;
      long ptype = PositionGetInteger(POSITION_TYPE);
      body += StringFormat("POS %s|%s|%s|%s|%s|%s|%s|%s|%s|%s|0|%I64d|%I64d|%s\n",
                           (string)ticket,
                           PositionGetString(POSITION_SYMBOL),
                           (ptype == POSITION_TYPE_BUY ? "0" : "1"),
                           DoubleToStr8(PositionGetDouble(POSITION_VOLUME)),
                           DoubleToStr8(PositionGetDouble(POSITION_PRICE_OPEN)),
                           DoubleToStr8(PositionGetDouble(POSITION_PRICE_CURRENT)),
                           DoubleToStr8(PositionGetDouble(POSITION_SL)),
                           DoubleToStr8(PositionGetDouble(POSITION_TP)),
                           DoubleToStr8(PositionGetDouble(POSITION_PROFIT)),
                           DoubleToStr8(PositionGetDouble(POSITION_SWAP)),
                           PositionGetInteger(POSITION_TIME),
                           PositionGetInteger(POSITION_MAGIC),
                           San(PositionGetString(POSITION_COMMENT)));
     }

   // ---- 挂单 ----
   int totalOrd = OrdersTotal();
   for(int i = 0; i < totalOrd; i++)
     {
      ulong ticket = OrderGetTicket(i);
      if(ticket == 0) continue;
      body += StringFormat("ORD %s|%s|%s|%s|%s|%s|%s|%I64d|%I64d|%s\n",
                           (string)ticket,
                           OrderGetString(ORDER_SYMBOL),
                           OrderKindText(OrderGetInteger(ORDER_TYPE)),
                           DoubleToStr8(OrderGetDouble(ORDER_VOLUME_INITIAL)),
                           DoubleToStr8(OrderGetDouble(ORDER_PRICE_OPEN)),
                           DoubleToStr8(OrderGetDouble(ORDER_SL)),
                           DoubleToStr8(OrderGetDouble(ORDER_TP)),
                           OrderGetInteger(ORDER_TIME_SETUP),
                           OrderGetInteger(ORDER_MAGIC),
                           San(OrderGetString(ORDER_COMMENT)));
     }

   // ---- K 线系列 ----
   for(int s = 0; s < ArraySize(g_seriesKeys); s++)
     {
      // 未解析成功的品种(如启动时品种尚未出现)按周期重试
      if(StringLen(g_seriesSymbols[s]) == 0)
        {
         int colonPos = StringFind(g_seriesKeys[s], ":");
         if(colonPos > 0)
           {
            string req = StringSubstr(g_seriesKeys[s], 0, colonPos);
            g_seriesSymbols[s] = ResolveSymbol(req);
           }
        }
      if(StringLen(g_seriesSymbols[s]) == 0) continue;

      MqlRates rates[];
      int copied = CopyRates(g_seriesSymbols[s], g_seriesTf[s], 0, g_seriesBars[s], rates);
      if(copied <= 0)
        {
         if(VerboseLog) PrintFormat("PABridge: CopyRates %s (%s) 失败 err=%d",
                                    g_seriesKeys[s], g_seriesSymbols[s], GetLastError());
         continue;
        }
      body += "BAR " + g_seriesKeys[s] + "\n";
      int digits = (int)SymbolInfoInteger(g_seriesSymbols[s], SYMBOL_DIGITS);
      for(int b = 0; b < copied; b++)
        {
         body += StringFormat("BART %I64d,%s,%s,%s,%s,%s\n",
                              (long)rates[b].time,
                              DoubleToStrN(rates[b].open, digits),
                              DoubleToStrN(rates[b].high, digits),
                              DoubleToStrN(rates[b].low, digits),
                              DoubleToStrN(rates[b].close, digits),
                              DoubleToStr8((double)rates[b].tick_volume));
        }
     }

   // ---- 执行结果 ----
   for(int r = 0; r < ArraySize(g_resQueue); r++)
      body += "RES id=" + g_resQueue[r] + "\n";
   ArrayResize(g_resQueue, 0);

   body += "END\n";
   return body;
  }

//+------------------------------------------------------------------+
//| 处理响应指令 (A2T v1)                                               |
//+------------------------------------------------------------------+
void ProcessCommands(const string response)
  {
   string lines[];
   int n = StringSplit(response, '\n', lines);
   for(int i = 0; i < n; i++)
     {
      string line = lines[i];
      StringTrimLeft(line);
      StringTrimRight(line);
      if(StringLen(line) == 0) continue;
      if(StringFind(line, "CMD ") != 0) continue;

      string kv = StringSubstr(line, 4);
      string id = KvGet(kv, "id");
      string action = KvGet(kv, "action");
      if(id == "" || action == "") continue;

      string cached;
      if(AlreadyDone(id, cached))
        {
         if(VerboseLog) PrintFormat("PABridge: 指令 %s 重复，回放缓存结果", id);
         QueueAlreadyDone(id, cached);
         continue;
        }

      string res = ExecuteCommand(id, action, kv);
      MarkDone(id, res);
     }
  }

//+------------------------------------------------------------------+
//| 重复指令: 仅回放结果                                                |
//+------------------------------------------------------------------+
void QueueAlreadyDone(const string id, const string cached)
  {
   if(StringLen(cached) == 0)
     {
      QueueResult(id, true, "duplicate", "");
      return;
     }
   int n = ArraySize(g_resQueue);
   ArrayResize(g_resQueue, n + 1);
   g_resQueue[n] = cached;
  }

//+------------------------------------------------------------------+
//| 执行单条指令，返回 RES 负载 "id|ok|msg|data"                          |
//+------------------------------------------------------------------+
string ExecuteCommand(const string id, const string action, const string kv)
  {
   bool ok = false;
   string msg = "";
   string data = "";

   if(action == "MARKET" || action == "LIMIT" || action == "STOP")
     {
      string sym = ResolveSymbol(KvGet(kv, "symbol"));
      string side = KvGet(kv, "side");
      double vol = StringToDouble(KvGet(kv, "volume"));
      double price = StringToDouble(KvGet(kv, "price"));
      double sl = StringToDouble(KvGet(kv, "sl"));
      double tp = StringToDouble(KvGet(kv, "tp"));
      string comment = KvGet(kv, "comment");

      if(sym == "")
        {
         QueueResult(id, false,
                     StringFormat("品种 %s 在当前 MT5 终端不存在 (注意大小写与经纪商后缀, 如 XAUUSDm)",
                                  KvGet(kv, "symbol")), "");
         return LastResPayload(id);
        }
      if(vol <= 0)
        {
         QueueResult(id, false, "参数错误: volume", "");
         return LastResPayload(id);
        }

      bool sent = false;
      if(action == "MARKET")
         sent = (side == "buy") ? trade.Buy(vol, sym, 0.0, sl, tp, comment)
                                : trade.Sell(vol, sym, 0.0, sl, tp, comment);
      else if(action == "LIMIT")
         sent = (side == "buy") ? trade.BuyLimit(vol, price, sym, sl, tp, ORDER_TIME_GTC, 0, comment)
                                : trade.SellLimit(vol, price, sym, sl, tp, ORDER_TIME_GTC, 0, comment);
      else
         sent = (side == "buy") ? trade.BuyStop(vol, price, sym, sl, tp, ORDER_TIME_GTC, 0, comment)
                                : trade.SellStop(vol, price, sym, sl, tp, ORDER_TIME_GTC, 0, comment);

      uint rc = trade.ResultRetcode();
      ok = sent && (rc == TRADE_RETCODE_DONE || rc == TRADE_RETCODE_PLACED || rc == TRADE_RETCODE_DONE_PARTIAL);
      msg = StringFormat("%s %s %s %.2f @ %s: retcode=%u %s",
                         action, side, sym, vol,
                         (price > 0 ? DoubleToString(price, (int)SymbolInfoInteger(sym, SYMBOL_DIGITS)) : "market"),
                         rc, trade.ResultRetcodeDescription());
      data = (ok ? (string)trade.ResultOrder() : "");
      if(VerboseLog || !ok) Print("PABridge: ", msg);
     }
   else if(action == "CANCEL")
     {
      ulong ticket = (ulong)StringToInteger(KvGet(kv, "ticket"));
      ok = trade.OrderDelete(ticket);
      msg = StringFormat("CANCEL #%I64u: retcode=%u %s", ticket, trade.ResultRetcode(), trade.ResultRetcodeDescription());
      data = (ok ? (string)ticket : "");
     }
   else if(action == "MODIFY")
     {
      ulong ticket = (ulong)StringToInteger(KvGet(kv, "ticket"));
      string slText = KvGet(kv, "sl");
      string tpText = KvGet(kv, "tp");
      if(!PositionSelectByTicket(ticket))
        {
         QueueResult(id, false, StringFormat("持仓 #%I64u 不存在", ticket), "");
         return LastResPayload(id);
        }
      double curSl = PositionGetDouble(POSITION_SL);
      double curTp = PositionGetDouble(POSITION_TP);
      double newSl = (StringLen(slText) > 0) ? StringToDouble(slText) : curSl;
      double newTp = (StringLen(tpText) > 0) ? StringToDouble(tpText) : curTp;
      ok = trade.PositionModify(ticket, newSl, newTp);
      msg = StringFormat("MODIFY #%I64u SL=%s TP=%s: retcode=%u %s",
                         ticket, DoubleToString(newSl, 8), DoubleToString(newTp, 8),
                         trade.ResultRetcode(), trade.ResultRetcodeDescription());
      data = (ok ? (string)ticket : "");
      if(VerboseLog || !ok) Print("PABridge: ", msg);
     }
   else if(action == "CLOSE")
     {
      ulong ticket = (ulong)StringToInteger(KvGet(kv, "ticket"));
      ok = trade.PositionClose(ticket);
      msg = StringFormat("CLOSE #%I64u: retcode=%u %s", ticket, trade.ResultRetcode(), trade.ResultRetcodeDescription());
      data = (ok ? (string)ticket : "");
      if(VerboseLog || !ok) Print("PABridge: ", msg);
     }
   else if(action == "SUBSCRIBE")
     {
      string series = KvGet(kv, "series");
      int bars = (int)StringToInteger(KvGet(kv, "bars"));
      int colon = StringFind(series, ":");
      if(colon > 0)
        {
         string symReq = StringSubstr(series, 0, colon);
         string tf = StringSubstr(series, colon + 1);
         string resolved = ResolveSymbol(symReq);
         SubscribeSeries(series, resolved, TfTextToEnum(tf), bars);
         if(resolved != "") AddSpecSymbol(resolved);
         if(resolved != "")
           {
            ok = true;
            msg = "已订阅 " + series + " (" + resolved + ")";
           }
         else
           {
            // 保留系列待重试, 但明确告知代理尚未找到该品种
            ok = false;
            msg = StringFormat("品种 %s 暂未在终端找到 (已保留订阅待重试; 注意大小写与后缀, 如 XAUUSDm)", symReq);
           }
        }
      else
        {
         ok = false;
         msg = "SUBSCRIBE series 格式错误: " + series;
        }
     }
   else if(action == "UNSUBSCRIBE")
     {
      string series = KvGet(kv, "series");
      UnsubscribeSeries(series);
      ok = true;
      msg = "已退订 " + series;
     }
   else if(action == "SPEC")
     {
      string resolved = ResolveSymbol(KvGet(kv, "symbol"));
      if(resolved != "")
        {
         AddSpecSymbol(resolved);
         ok = true;
         msg = "已加入规格上报 " + resolved;
        }
      else
        {
         ok = false;
         msg = StringFormat("品种 %s 在当前 MT5 终端不存在 (注意大小写与经纪商后缀, 如 XAUUSDm)",
                            KvGet(kv, "symbol"));
        }
     }
   else
     {
      QueueResult(id, false, "未知指令: " + action, "");
      return LastResPayload(id);
     }

   QueueResult(id, ok, msg, data);
   return LastResPayload(id);
  }

//+------------------------------------------------------------------+
//| 取最近入队结果的负载(供 MarkDone 缓存)                               |
//+------------------------------------------------------------------+
string LastResPayload(const string id)
  {
   int n = ArraySize(g_resQueue);
   if(n > 0 && StringFind(g_resQueue[n - 1], id + "|") == 0)
      return g_resQueue[n - 1];
   return id + "|0|unknown|";
  }

//+------------------------------------------------------------------+
//| 订单类型文本                                                        |
//+------------------------------------------------------------------+
string OrderKindText(const long t)
  {
   switch((int)t)
     {
      case ORDER_TYPE_BUY: return "buy";
      case ORDER_TYPE_SELL: return "sell";
      case ORDER_TYPE_BUY_LIMIT: return "buy_limit";
      case ORDER_TYPE_SELL_LIMIT: return "sell_limit";
      case ORDER_TYPE_BUY_STOP: return "buy_stop";
      case ORDER_TYPE_SELL_STOP: return "sell_stop";
      case ORDER_TYPE_BUY_STOP_LIMIT: return "buy_stoplimit";
      case ORDER_TYPE_SELL_STOP_LIMIT: return "sell_stoplimit";
     }
   return "unknown";
  }

//+------------------------------------------------------------------+
//| 数字格式化                                                          |
//+------------------------------------------------------------------+
string DoubleToStr8(const double v)
  {
   return DoubleToString(v, 8);
  }

string DoubleToStrN(const double v, const int digits)
  {
   return DoubleToString(v, digits);
  }

//+------------------------------------------------------------------+
//| 图表状态注释                                                        |
//+------------------------------------------------------------------+
void UpdateComment()
  {
   string status = g_lastError == "" ? "已连接" : ("错误: " + g_lastError);
   string text = StringFormat(
      "PABridge 运行中\n状态: %s\n同步次数: %d\n上次同步: %s\nK线系列: %d 个\n品种规格: %d 个\n魔术号: %I64d",
      status, g_syncCount,
      TimeToString(g_lastSyncTime, TIME_DATE | TIME_SECONDS),
      ArraySize(g_seriesKeys), ArraySize(g_specSymbols), MagicNumber);
   Comment(text);
  }
//+------------------------------------------------------------------+
