//! 权益曲线数据模块：定时采样 → jsonl 落盘 → 时间段查询。
//!
//! 设计要点（方案 A，2026-09-08）：
//! - 每 60s 从 BridgeState 采样一次账户快照（equity/balance/unrealized），无需额外请求 EA；
//! - 按天追加写 `records/equity/YYYYMMDD.jsonl`，重启后启动时回填内存；
//! - 查询按 [from, to] 过滤并降采样（桶平均）到前端可绘制的点数；
//! - 时间统一存 Unix 毫秒（UTC），文件名用本地日期，前端按本地时区显示。

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::web::handlers::AppState;

/// 采样间隔（秒）。
pub const SAMPLE_INTERVAL_SECS: u64 = 60;
/// 内存最多保留的点数（60s 一点约 34 天）。
const MAX_MEMORY_POINTS: usize = 50_000;
/// 查询默认/最大返回点数（与前端 drawEquityChart 的绘制容量匹配）。
pub const DEFAULT_QUERY_POINTS: usize = 160;
pub const MAX_QUERY_POINTS: usize = 500;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EquityPoint {
    /// Unix 毫秒（UTC）。
    pub ts: i64,
    /// 总权益（account.equity）。
    pub value: f64,
    /// 余额（account.balance）。
    pub balance: f64,
    /// 浮动盈亏（account.profit）。
    pub unrealized: f64,
}

pub struct EquityLog {
    dir: PathBuf,
    points: RwLock<Vec<EquityPoint>>,
}

impl EquityLog {
    /// 创建并从磁盘回填（最近 MAX_MEMORY_POINTS 点）。
    pub fn new(dir: PathBuf) -> Self {
        let _ = fs::create_dir_all(&dir);
        let mut points = Self::load_from_disk(&dir);
        if points.len() > MAX_MEMORY_POINTS {
            points = points.split_off(points.len() - MAX_MEMORY_POINTS);
        }
        Self { dir, points: RwLock::new(points) }
    }

    /// 记录一个采样点：内存 + 按天 jsonl 追加。
    pub fn record(&self, point: EquityPoint) {
        let file = self.day_file(point.ts);
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&file) {
            if let Ok(line) = serde_json::to_string(&point) {
                let _ = writeln!(f, "{}", line);
            }
        }
        let mut pts = self.points.write();
        pts.push(point);
        let len = pts.len();
        if len > MAX_MEMORY_POINTS {
            pts.drain(..len - MAX_MEMORY_POINTS);
        }
    }

    /// 查询 [from_ms, to_ms] 区间内的点，超出 max_points 时按桶平均降采样。
    pub fn query(&self, from_ms: i64, to_ms: i64, max_points: usize) -> Vec<EquityPoint> {
        let max_points = max_points.clamp(10, MAX_QUERY_POINTS);
        let (from_ms, to_ms) = if from_ms <= to_ms { (from_ms, to_ms) } else { (to_ms, from_ms) };
        let selected: Vec<EquityPoint> = self
            .points
            .read()
            .iter()
            .filter(|p| p.ts >= from_ms && p.ts <= to_ms)
            .cloned()
            .collect();
        if selected.len() <= max_points {
            return selected;
        }
        downsample(&selected, max_points)
    }

    /// 最近一个采样点时间（用于前端判断数据新鲜度）。
    pub fn last_ts(&self) -> Option<i64> {
        self.points.read().last().map(|p| p.ts)
    }

    /// 采样点总数（诊断用）。
    pub fn len(&self) -> usize {
        self.points.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 采样落盘的目录。
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn day_file(&self, ts_ms: i64) -> PathBuf {
        let day = chrono::DateTime::from_timestamp_millis(ts_ms)
            .map(|d| d.with_timezone(&chrono::Local))
            .unwrap_or_else(chrono::Local::now);
        self.dir.join(format!("{}.jsonl", day.format("%Y%m%d")))
    }

    fn load_from_disk(dir: &Path) -> Vec<EquityPoint> {
        let mut files: Vec<PathBuf> = fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
                    .collect()
            })
            .unwrap_or_default();
        files.sort();
        let mut points = Vec::new();
        for file in files {
            if let Ok(f) = File::open(&file) {
                for line in BufReader::new(f).lines().map_while(Result::ok) {
                    if let Ok(p) = serde_json::from_str::<EquityPoint>(line.trim()) {
                        points.push(p);
                    }
                }
            }
        }
        points.sort_by_key(|p| p.ts);
        points
    }
}

/// 等宽桶降采样。
///
/// - `ts` / `balance` / `unrealized` 取桶内**最后一个点**：余额只在平仓、出入金时跳变，
///   是阶梯量，取均值会把它抹成斜坡；ts 取末值也才真正代表"截至该时刻"。
/// - `value`（净值）取桶内**均值**：它是连续曲线，均值可抑制采样噪声。
fn downsample(points: &[EquityPoint], target: usize) -> Vec<EquityPoint> {
    if points.len() <= target || target == 0 {
        return points.to_vec();
    }
    let bucket = points.len() as f64 / target as f64;
    let mut out = Vec::with_capacity(target);
    let mut start = 0usize;
    for i in 0..target {
        let end = (((i + 1) as f64) * bucket).round() as usize;
        let end = end.clamp(start + 1, points.len());
        let slice = &points[start..end];
        let n = slice.len() as f64;
        let last = &slice[slice.len() - 1];
        out.push(EquityPoint {
            ts: last.ts,
            value: slice.iter().map(|p| p.value).sum::<f64>() / n,
            balance: last.balance,
            unrealized: last.unrealized,
        });
        start = end;
        if start >= points.len() {
            break;
        }
    }
    out
}

#[derive(Debug, Deserialize)]
pub struct EquityQuery {
    pub from: Option<i64>,
    pub to: Option<i64>,
    pub points: Option<usize>,
}

/// GET /api/account/equity?from=&to=&points= —— 登录即可查看（只读账号可看曲线）。
pub async fn handle_equity_curve(
    State(service): State<AppState>,
    Query(q): Query<EquityQuery>,
) -> Response {
    let to_ms = q.to.unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
    let from_ms = q
        .from
        .unwrap_or(to_ms - 24 * 3600 * 1000);
    let points = service.equity_curve(from_ms, to_ms, q.points.unwrap_or(DEFAULT_QUERY_POINTS));
    Json(json!({
        "points": points,
        "from": from_ms,
        "to": to_ms,
        "count": points.len(),
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(ts: i64, value: f64) -> EquityPoint {
        EquityPoint { ts, value, balance: value - 10.0, unrealized: 10.0 }
    }

    #[test]
    fn test_record_query_and_persistence() {
        let dir = std::env::temp_dir().join(format!("equity_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        {
            let log = EquityLog::new(dir.clone());
            for i in 0..5 {
                log.record(point(1_700_000_000_000 + i * 60_000, 10_000.0 + i as f64));
            }
            assert_eq!(log.len(), 5);
            let got = log.query(1_700_000_000_000, 1_700_000_300_000, 160);
            assert_eq!(got.len(), 5);
            assert!((got[4].value - 10_004.0).abs() < 1e-9);
        }
        // 重启后从磁盘回填
        let log2 = EquityLog::new(dir.clone());
        assert_eq!(log2.len(), 5);
        assert_eq!(log2.last_ts(), Some(1_700_000_240_000));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_query_range_filter() {
        let dir = std::env::temp_dir().join(format!("equity_rng_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let log = EquityLog::new(dir.clone());
        for i in 0..10 {
            log.record(point(1_700_000_000_000 + i * 60_000, 100.0));
        }
        let got = log.query(1_700_000_120_000, 1_700_000_300_000, 160);
        assert_eq!(got.len(), 4); // ts 2..5 分钟内（含端点 120s,180s,240s,300s）
        assert_eq!(got[0].ts, 1_700_000_120_000);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_downsample_bucket_average() {
        let pts: Vec<EquityPoint> = (0..100)
            .map(|i| point(1_000_000 + i * 1000, i as f64))
            .collect();
        let out = downsample(&pts, 10);
        assert_eq!(out.len(), 10);
        // 第 0 桶 = 0..10 均值 4.5
        assert!((out[0].value - 4.5).abs() < 1e-9);
        // ts 单调不减
        assert!(out.windows(2).all(|w| w[0].ts <= w[1].ts));
    }

    #[test]
    fn test_downsample_keeps_balance_step_shape() {
        // 余额是阶梯量：桶内均值会把它抹成斜坡，必须取桶内末值
        let mut pts = Vec::new();
        for i in 0..100 {
            let balance = if i < 50 { 10_000.0 } else { 11_000.0 };
            pts.push(EquityPoint {
                ts: 1_000_000 + i * 1000,
                value: balance + 5.0,
                balance,
                unrealized: 5.0,
            });
        }
        let out = downsample(&pts, 10);
        assert_eq!(out.len(), 10);
        // 阶梯必须保住：只允许出现两个真实台阶值，不得出现中间斜坡值
        assert!(
            out.iter().all(|p| p.balance == 10_000.0 || p.balance == 11_000.0),
            "余额被平滑成了斜坡: {:?}",
            out.iter().map(|p| p.balance).collect::<Vec<_>>()
        );
        assert_eq!(out.last().unwrap().balance, 11_000.0);
        // 净值仍为桶均值（0..10 -> 10_005.0）
        assert!((out[0].value - 10_005.0).abs() < 1e-9);
        // ts 与余额同取桶内末值，时序自洽
        assert!(out.windows(2).all(|w| w[0].ts <= w[1].ts));
    }

    #[test]
    fn test_query_no_data_returns_empty() {
        let dir = std::env::temp_dir().join(format!("equity_empty_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let log = EquityLog::new(dir.clone());
        assert!(log.query(0, i64::MAX, 160).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
