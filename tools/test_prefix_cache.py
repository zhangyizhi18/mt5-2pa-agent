# -*- coding: utf-8 -*-
"""
前缀缓存实测：同一段长前缀连发两次，比较预填充耗时。
若第二次显著更快 -> 服务端隐式 Prompt Cache 生效（开关①无需改动）。
"""
import json
import os
import time
import urllib.request

API_KEY = os.environ.get("LLM_API_KEY", "").strip()
BASE = os.environ.get("LLM_BASE_URL", "https://api.xiaomimimo.com/v1").rstrip("/")
MODEL = os.environ.get("LLM_MODEL", "mimo-v2.5")

# 造一段固定长前缀（约 3 万字），模拟“市场诊断框架”那类常驻提示词
PARA = (
    "价格行为分析规则：市场周期可分为尖峰、紧通道、宽通道、震荡区间、趋势性交易区间等形态。"
    "在判断多空主导力量时，应结合 K 线实体比例、影线长度、缺口方向与位置关系综合评估。"
    "当出现趋势K线时，需确认是否处于突破后的延续阶段；若为十字星则观察其上下影线的对称性。"
)
PREFIX = (PARA * 200)  # 200 段 ≈ 3 万字


def call(tag, suffix, prefix=None):
    body = {
        "model": MODEL,
        "messages": [
            {"role": "user", "content": (prefix or PREFIX) + "\n\n" + suffix},
        ],
        "stream": False,
        "max_tokens": 16,      # 只生成极短输出，把耗时集中暴露在预填充上
        "temperature": 0,
    }
    req = urllib.request.Request(
        BASE + "/chat/completions",
        data=json.dumps(body).encode("utf-8"),
        headers={
            "Content-Type": "application/json",
            "Authorization": "Bearer " + API_KEY,
        },
    )
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))  # 绕过系统代理
    t0 = time.time()
    try:
        with opener.open(req, timeout=300) as r:
            raw = r.read().decode("utf-8")
    except Exception as e:                                     # noqa: BLE001
        print(f"[{tag}] 请求失败: {type(e).__name__} {e}")
        return None
    dt = time.time() - t0
    d = json.loads(raw)
    u = d.get("usage", {}) or {}
    print(f"[{tag}] 耗时 {dt:6.2f}s  "
          f"usage={json.dumps(u, ensure_ascii=False)}")
    print(f"        响应顶层字段: {list(d.keys())}")
    return dt


if __name__ == "__main__":
    if not API_KEY:
        raise SystemExit("未设置 LLM_API_KEY")
    print(f"模型 {MODEL}  前缀长度 {len(PREFIX):,} 字符  端点 {BASE}\n")
    t1 = call("第1次-冷", "请只回复：收到")
    time.sleep(1)
    t2 = call("第2次-同前缀", "请只回复：好")
    time.sleep(1)
    # 对照组：换成另一段同长度前缀，命中率应为 0
    other = ("完全不同的另一套规则说明，用于对照测试缓存是否真的按前缀匹配。" * 200)[:len(PREFIX)]
    t3 = call("第3次-换前缀(对照)", "请只回复：行", prefix=other)
    if t1 and t2:
        print(f"\n同前缀第2次比第1次 {'快' if t2 < t1 else '慢'} "
              f"{abs(t1 - t2):.2f}s ({abs(t1 - t2) / t1 * 100:.0f}%)")
