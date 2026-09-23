# -*- coding: utf-8 -*-
"""涨跌配色切换的真实渲染验证。

用 static/app.css 的真实样式 + static/app.js 的真实函数（cssVar/updownColors/applyUpdown/
initUpdownToggle）在 Chrome 无头里渲染两种配色，并把「点击切换」的全过程跑一遍：
  - data-updown 是否真的翻转
  - localStorage 是否落盘
  - 切换后是否触发了 K 线图与权益图重绘（最容易漏的一步）

产出：promo/updown_cn.png、promo/updown_intl.png（各含 CSS 类样例 + canvas 取色样例 + 自检行）

用法：python tools/make_updown_preview.py
"""
import functools
import http.server
import io
import os
import re
import shutil
import socketserver
import subprocess
import sys
import tempfile
import threading

sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding="utf-8", errors="replace")

from PIL import Image, ImageDraw, ImageFont

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
STATIC = os.path.join(ROOT, "static")
OUTDIR = os.path.join(ROOT, "promo")
CHROME = r"C:\Program Files\Google\Chrome\Application\chrome.exe"
FONT = r"C:\Windows\Fonts\msyh.ttc"
VIEW_W, VIEW_H = 1120, 780
SCALE = 2
SHELL_W = 1040


def js_function(src, name):
    idx = src.find("function " + name + "(")
    if idx < 0:
        raise SystemExit("未找到函数 " + name)
    depth = 0
    for j in range(src.find("{", idx), len(src)):
        if src[j] == "{":
            depth += 1
        elif src[j] == "}":
            depth -= 1
            if depth == 0:
                return src[idx : j + 1] + ";"
    raise SystemExit("函数体未闭合 " + name)


def js_const(src, name):
    m = re.search(r"^const %s = .*$" % name, src, re.M)
    if not m:
        raise SystemExit("未找到常量 " + name)
    return m.group(0)


FUNCS = ["cssVar", "updownColors", "invalidateUpdownColors", "upColor", "downColor",
         "currentUpdown", "applyUpdown", "initUpdownToggle"]


def build_html(mode):
    css = open(os.path.join(STATIC, "app.css"), encoding="utf-8").read()
    src = open(os.path.join(STATIC, "app.js"), encoding="utf-8").read()
    page = open(os.path.join(STATIC, "index.html"), encoding="utf-8").read()

    # 取真实顶栏（含真实按钮 DOM 与文案），保证验证的就是线上那套
    start = page.find('<div class="status-row">')
    end = page.find("</div>", start) + len("</div>")
    status_row = page[start:end]
    for hidden_id in ('id="credentialBadge"', 'id="userBadge"', 'id="userMgmtButton"',
                      'id="logoutButton"'):
        status_row = status_row.replace(" " + hidden_id + " hidden", " " + hidden_id + " hidden")

    bundle = "\n".join(
        [js_const(src, n) for n in ("EQUITY_NET_COLOR", "EQUITY_LOSS_COLOR", "UPDOWN_KEY")]
        + [js_function(src, n) for n in FUNCS]
    )

    harness = """
const log = [];
const toggle = document.getElementById("updownToggle");
let chartRedraws = 0, equityRedraws = 0;
function drawChart() { chartRedraws += 1; }
function drawEquityChart() { equityRedraws += 1; }
const $ = (id) => document.getElementById(id);
__BUNDLE__

// 页面用 http 提供（file:// 下 localStorage 会抛 SecurityError，验证不了持久化）
try { localStorage.removeItem("mt5_updown"); } catch (e) {}
let lsNote = "localStorage 可用";
try { localStorage.setItem("__probe", "1"); localStorage.removeItem("__probe"); }
catch (e) { lsNote = "localStorage 不可用: " + e.name; }

// 抽取清单漏了函数/常量时直接报出来，否则会静默出现「setItem(undefined, …)」这种假通过
const REQUIRED = ["cssVar", "updownColors", "invalidateUpdownColors", "upColor", "downColor",
                  "currentUpdown", "applyUpdown", "initUpdownToggle", "UPDOWN_KEY", "EQUITY_NET_COLOR"];
const MISSING = REQUIRED.filter((name) => {
  try { return typeof eval(name) === "undefined"; } catch (e) { return true; }
});

initUpdownToggle();
log.push(MISSING.length ? ("✗ 抽取清单缺少: " + MISSING.join(", ")) : "✓ 抽取清单完整（10 项）");
log.push("初始按钮文案: " + toggle.textContent);
log.push("初始 data-updown: " + document.documentElement.dataset.updown);
log.push(lsNote);

// 真实点击一次，验证切换链路（改属性 → 落盘 → 失效取色缓存 → 重绘两张 canvas 图）
toggle.click();
log.push("点击后 data-updown: " + document.documentElement.dataset.updown);
log.push("点击后 localStorage mt5_updown = " + localStorage.getItem("mt5_updown"));
log.push("点击后按钮文案: " + toggle.textContent);
log.push("重绘次数: drawChart=" + chartRedraws + " · drawEquityChart=" + equityRedraws);
const after = updownColors();
log.push("点击后取色: 涨=" + after.up + " · 跌=" + after.down);

// 点击验证完毕后回到本图要展示的配色，保证画面与顶栏文案一致
applyUpdown("__MODE__", false);
if (document.documentElement.dataset.updown === "__MODE__") {
  log.push("✓ 本图展示配色 = __MODE__");
} else {
  log.push("✗ 本图展示配色不是 __MODE__");
}

// 画色块：canvas 不能解析 var()，这里验证的正是「取到的真实颜色值」
function paint() {
  const colors = updownColors();
  const rows = [
    ["涨（--up）", colors.up],
    ["跌（--down）", colors.down],
    ["浮盈带", colors.upBand],
    ["净值线（身份色，不受开关影响）", EQUITY_NET_COLOR],
  ];
  const canvas = document.getElementById("swatch");
  const dpr = window.devicePixelRatio || 1;
  const W = canvas.parentElement.clientWidth, H = 172;
  canvas.width = W * dpr; canvas.height = H * dpr;
  canvas.style.width = W + "px"; canvas.style.height = H + "px";
  const c = canvas.getContext("2d");
  c.scale(dpr, dpr);
  c.clearRect(0, 0, W, H);
  c.font = "12px Segoe UI";
  rows.forEach(([label, color], i) => {
    const y = 10 + i * 40;
    c.fillStyle = color;
    c.fillRect(0, y, 46, 26);
    c.strokeStyle = "#3a4245";
    c.lineWidth = 1;
    c.strokeRect(0.5, y + 0.5, 46, 26);
    c.fillStyle = "#aab4b8";
    c.fillText(label + "  " + color, 56, y + 18);
  });
  // 蜡烛 + 浮盈带：用真实取色画，模拟 K 线图与权益图的着色
  const bx = 330, by = 10;
  c.fillStyle = colors.up;
  c.fillRect(bx, by, 16, 52);
  c.fillStyle = colors.down;
  c.fillRect(bx + 30, by, 16, 52);
  c.fillStyle = "#aab4b8";
  c.fillText("K线蜡烛（涨/跌）", bx, by + 68);
  c.fillStyle = colors.upBand;
  c.fillRect(bx + 140, by + 10, 90, 30);
  c.fillStyle = colors.downBand;
  c.fillRect(bx + 140, by + 50, 90, 18);
  c.strokeStyle = EQUITY_NET_COLOR; c.lineWidth = 2;
  c.beginPath(); c.moveTo(bx + 140, by + 8); c.lineTo(bx + 230, by + 8); c.stroke();
  c.fillStyle = "#aab4b8";
  c.fillText("权益：净线+浮盈带", bx + 140, by + 86);
}
paint();

document.getElementById("selfcheck").innerHTML = log
  .map((line) => "<div>" + line + "</div>").join("");
"""
    harness = harness.replace("__BUNDLE__", bundle).replace("__MODE__", mode)

    return """<!DOCTYPE html><html lang="zh-CN" data-updown="%(mode)s"><head><meta charset="utf-8"><style>
%(css)s
body{background:#0b0d0e;margin:0;padding:16px %(pad)dpx}
.preview-shell{width:%(shell)dpx}
.panel{background:var(--panel);border:1px solid var(--line);border-radius:6px;padding:16px 18px}
.panel h3{margin:0 0 12px;font-size:13px;color:var(--muted);font-weight:600}
.samples{display:grid;grid-template-columns:repeat(2,1fr);gap:10px 26px;margin-bottom:14px}
.samples>*{display:flex;align-items:center;gap:10px;font-size:13px}
.samples .k{color:var(--muted);font-size:11px;min-width:96px}
.bar{height:22px;border-radius:3px}
#selfcheck{margin-top:14px;border-top:1px solid var(--line);padding-top:12px;
 font:12px/1.75 Consolas,monospace;color:#9fb0b6}
#selfcheck .ok{color:#1fc48d}
</style></head><body><div class="preview-shell">
<div class="panel">
  <h3>顶栏（真实 DOM，含切换按钮，已在 %(label)s 模式下）</h3>
  %(row)s
  <h3 style="margin-top:16px">CSS 类样例（读 var(--up)/var(--down)，换配色即时生效）</h3>
  <div class="samples">
    <div><span class="k">.positive 盈亏</span><b class="positive">+1,234.56</b></div>
    <div><span class="k">.negative 盈亏</span><b class="negative">-234.56</b></div>
    <div><span class="k">.direction.long</span><b class="direction long">做多</b></div>
    <div><span class="k">.direction.short</span><b class="direction short">做空</b></div>
    <div><span class="k">.side-label.long</span><span class="side-label long">LONG</span></div>
    <div><span class="k">.side-label.short</span><span class="side-label short">SHORT</span></div>
    <div><span class="k">.history-direction</span><span class="history-direction long">买入开仓</span></div>
    <div><span class="k">周期涨跌幅</span><b style="color:var(--up)">+0.42%%</b> <b style="color:var(--down)">-0.31%%</b></div>
  </div>
  <h3>canvas 取色样例（canvas 不认 var()，必须取真实值 —— 最易漏的一环）</h3>
  <div style="position:relative;height:172px"><canvas id="swatch"></canvas></div>
  <div id="selfcheck"></div>
</div>
</div>
<script>%(harness)s</script></body></html>""" % {
        "mode": mode,
        "label": "默认红涨绿跌" if mode == "cn" else "绿涨红跌",
        "css": css,
        "pad": 16,
        "shell": SHELL_W,
        "row": status_row,
        "harness": harness,
    }


def start_server(directory):
    """起一个只监听回环的静态服务：file:// 下 localStorage 会抛 SecurityError，
    持久化这一步就永远验证不了，所以必须走 http。"""
    class Handler(http.server.SimpleHTTPRequestHandler):
        def log_message(self, *args):
            pass

    handler = functools.partial(Handler, directory=directory)
    httpd = socketserver.TCPServer(("127.0.0.1", 0), handler)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    return httpd, httpd.server_address[1]


def shoot(url, png_path, profile):
    cmd = [
        CHROME, "--headless=new", "--disable-gpu", "--hide-scrollbars",
        "--no-first-run", "--no-default-browser-check", "--no-proxy-server",
        f"--user-data-dir={profile}",
        f"--force-device-scale-factor={SCALE}",
        f"--window-size={VIEW_W},{VIEW_H}",
        "--virtual-time-budget=3000",
        f"--screenshot={png_path}", url,
    ]
    subprocess.run(cmd, capture_output=True, text=True, timeout=180)
    if not os.path.exists(png_path):
        raise SystemExit("截图失败：" + png_path)


def label(img, text):
    font = ImageFont.truetype(FONT, 22)
    band = Image.new("RGB", (img.width, 40), (11, 13, 14))
    ImageDraw.Draw(band).text((4, 8), text, font=font, fill=(137, 147, 151))
    out = Image.new("RGB", (img.width, img.height + 40), (11, 13, 14))
    out.paste(band, (0, 0))
    out.paste(img, (0, 40))
    return out


def main():
    if not os.path.exists(CHROME):
        raise SystemExit("找不到 Chrome：" + CHROME)
    os.makedirs(OUTDIR, exist_ok=True)

    httpd, port = start_server(OUTDIR)
    profile = tempfile.mkdtemp(prefix="updown-preview-")
    print(f"本地服务 http://127.0.0.1:{port}/ （只监听回环）")
    try:
        shots = []
        for mode, filename, text in (
            ("cn", "updown_cn.png", "默认：红涨绿跌（国内习惯）"),
            ("intl", "updown_intl.png", "切换后：绿涨红跌（国际惯例）"),
        ):
            html_path = os.path.join(OUTDIR, "_updown_%s.html" % mode)
            png_path = os.path.join(OUTDIR, "_updown_%s.png" % mode)
            with open(html_path, "w", encoding="utf-8") as f:
                f.write(build_html(mode))
            url = f"http://127.0.0.1:{port}/_updown_{mode}.html"
            shoot(url, png_path, profile)
            img = Image.open(png_path).convert("RGB")
            labeled = label(img, text)
            labeled.save(os.path.join(OUTDIR, filename))
            shots.append(labeled)

        gap = 16
        width = max(s.width for s in shots)
        height = sum(s.height for s in shots) + gap
        out = Image.new("RGB", (width, height), (11, 13, 14))
        y = 0
        for s in shots:
            out.paste(s, (0, y))
            y += s.height + gap
        final = os.path.join(OUTDIR, "updown_compare.png")
        out.save(final)
        print("已生成", final, out.size)
        print("每张图底部自检行记录了：首屏文案与配色、点击后 data-updown、localStorage 落盘值、"
              "两张 canvas 图的重绘次数、点击后实际取到的颜色。")
    finally:
        httpd.shutdown()
        shutil.rmtree(profile, ignore_errors=True)


if __name__ == "__main__":
    main()
