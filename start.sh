#!/usr/bin/env bash
set -e

echo "===================================================="
echo "  MT5 2PA Agent (Rust High-Performance Edition)"
echo "  Starting server at http://127.0.0.1:8066 ..."
echo "===================================================="
echo "  Note: MetaTrader 5 + PABridge EA 通常运行在 Windows；"
echo "  可让 EA 的 BridgeUrl 指向本机局域网 IP 以跨机桥接。"

if [ -f "./target/release/mt5-2pa-agent" ]; then
    ./target/release/mt5-2pa-agent --host 127.0.0.1 --port 8066
else
    cargo run --release -- --host 127.0.0.1 --port 8066
fi
