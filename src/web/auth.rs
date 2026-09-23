//! Web 控制台账号与权限（非侵入式独立模块）。
//!
//! - 账号存储：`config/users.json`（口令为「盐 + 1 万轮 SHA-256」哈希，绝不存明文）。
//! - 内置角色：`admin`（全部权限）/ `readonly`（只读，仅允许 GET 查询）。
//! - 会话：内存态 token + HttpOnly Cookie（SameSite=Lax），12 小时滑动过期。
//! - 鉴权策略：axum 中间件按「路径 + HTTP 方法」统一裁决——
//!   除 `/bridge/sync`（EA 专用，走 BridgeToken）、登录接口与静态资源外，
//!   所有 `/api/*` 必须已登录；所有非 GET/HEAD 请求必须管理员角色。
//!   新增路由默认被保护，避免"漏配守卫"类漏洞。
//! - 登录限速：同一用户名连续失败 5 次锁定 5 分钟。

use axum::extract::{Path as AxumPath, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::Extension;
use chrono::{DateTime, Duration, Utc};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use crate::config::paths::config_dir;
use crate::web::handlers::AppState;

pub const SESSION_COOKIE: &str = "pa_session";
const SESSION_TTL_HOURS: i64 = 12;
const HASH_ITERATIONS: u32 = 10_000;
const MAX_LOGIN_FAILURES: u32 = 5;
const LOCKOUT_SECS: u64 = 300;

// ---------------------------------------------------------------------------
// 角色与账号记录
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Admin,
    #[serde(rename = "readonly")]
    ReadOnly,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::ReadOnly => "readonly",
        }
    }

    pub fn parse(value: &str) -> Option<Role> {
        match value.trim().to_lowercase().as_str() {
            "admin" => Some(Role::Admin),
            "readonly" | "read_only" | "viewer" => Some(Role::ReadOnly),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserRecord {
    pub username: String,
    pub role: Role,
    pub salt: String,
    pub password_hash: String,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct AuthUser {
    pub username: String,
    pub role: Role,
}

impl AuthUser {
    pub fn is_admin(&self) -> bool {
        self.role == Role::Admin
    }
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct UsersFile {
    users: Vec<UserRecord>,
}

// ---------------------------------------------------------------------------
// 口令哈希
// ---------------------------------------------------------------------------

/// 盐 + 1 万轮迭代 SHA-256（无需引入额外依赖）。
pub fn hash_password(salt: &str, password: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(salt.as_bytes());
    hasher.update(b"$");
    hasher.update(password.as_bytes());
    let mut out = hasher.finalize();
    for _ in 1..HASH_ITERATIONS {
        let mut hasher = Sha256::new();
        hasher.update(out);
        out = hasher.finalize();
    }
    hex::encode(out)
}

fn new_salt() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// 近似常数时间比较，避免逐字节短路造成的时序侧信道。
fn hash_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

// ---------------------------------------------------------------------------
// 认证服务
// ---------------------------------------------------------------------------

struct SessionInfo {
    username: String,
    role: Role,
    expires_at: DateTime<Utc>,
}

struct LoginAttempt {
    failures: u32,
    locked_until: Option<Instant>,
}

pub struct AuthService {
    users_path: PathBuf,
    users: RwLock<Vec<UserRecord>>,
    sessions: Mutex<HashMap<String, SessionInfo>>,
    login_attempts: Mutex<HashMap<String, LoginAttempt>>,
}

impl AuthService {
    /// 生产入口：加载 `config/users.json`，必要时引导创建内置账号。
    pub fn new() -> Self {
        Self::with_users_path(config_dir().join("users.json"), true)
    }

    /// 测试/定制入口：指定存储路径；`bootstrap=false` 时不创建内置账号。
    pub fn with_users_path(path: PathBuf, bootstrap: bool) -> Self {
        let service = Self {
            users_path: path,
            users: RwLock::new(Vec::new()),
            sessions: Mutex::new(HashMap::new()),
            login_attempts: Mutex::new(HashMap::new()),
        };
        service.load_from_disk();
        if bootstrap {
            service.bootstrap_builtin_users();
        }
        service
    }

    fn load_from_disk(&self) {
        let path = &self.users_path;
        let Ok(content) = std::fs::read_to_string(path) else {
            return;
        };
        match serde_json::from_str::<UsersFile>(&content) {
            Ok(file) => {
                *self.users.write() = file.users;
                tracing::info!("[账号] 已加载 {} 个用户 ({})", self.users.read().len(), path.display());
            }
            Err(e) => {
                tracing::error!("[账号] users.json 解析失败（将视为无账号）: {e}");
            }
        }
    }

    fn save_to_disk(&self) {
        let file = UsersFile {
            users: self.users.read().clone(),
        };
        if let Some(parent) = self.users_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match serde_json::to_string_pretty(&file) {
            Ok(content) => {
                if let Err(e) = std::fs::write(&self.users_path, content) {
                    tracing::error!("[账号] 写入 users.json 失败: {e}");
                }
            }
            Err(e) => tracing::error!("[账号] 序列化 users.json 失败: {e}"),
        }
    }

    /// users.json 不存在/没有对应账号时，用环境变量（或默认值）创建内置账号。
    fn bootstrap_builtin_users(&self) {
        let admin = env_or("WEB_ADMIN_USERNAME", "admin");
        let admin_pwd = env_or("WEB_ADMIN_PASSWORD", "admin123");
        let viewer = env_or("WEB_VIEWER_USERNAME", "viewer");
        let viewer_pwd = env_or("WEB_VIEWER_PASSWORD", "viewer123");

        let mut created_any = false;
        if self.find_user(&admin).is_none() {
            self.insert_user_raw(admin.clone(), Role::Admin, &admin_pwd);
            created_any = true;
            if admin_pwd == "admin123" {
                tracing::warn!("[账号] 已创建默认管理员 {} / {} —— 请登录后在「用户管理」中立即修改密码！", admin, admin_pwd);
            }
        }
        if self.find_user(&viewer).is_none() {
            self.insert_user_raw(viewer.clone(), Role::ReadOnly, &viewer_pwd);
            created_any = true;
            if viewer_pwd == "viewer123" {
                tracing::warn!("[账号] 已创建默认只读账号 {} / {} —— 请尽快修改密码！", viewer, viewer_pwd);
            }
        }
        if created_any {
            self.save_to_disk();
        }
    }

    fn find_user(&self, username: &str) -> Option<UserRecord> {
        self.users
            .read()
            .iter()
            .find(|u| u.username == username)
            .cloned()
    }

    fn insert_user_raw(&self, username: String, role: Role, password: &str) {
        let salt = new_salt();
        let record = UserRecord {
            username,
            role,
            salt: salt.clone(),
            password_hash: hash_password(&salt, password),
            created_at: Utc::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        };
        self.users.write().push(record);
    }

    // ----------------------- 登录 / 会话 -----------------------

    /// 登录（带失败锁定）。成功返回会话 token。
    pub fn attempt_login(&self, username: &str, password: &str) -> Result<(String, Role), String> {
        let username = username.trim();
        if username.is_empty() || password.is_empty() {
            return Err("用户名与密码不能为空".into());
        }

        // 锁定检查
        {
            let mut attempts = self.login_attempts.lock();
            if let Some(attempt) = attempts.get_mut(username) {
                if let Some(until) = attempt.locked_until {
                    if Instant::now() < until {
                        return Err("失败次数过多，该账号已被临时锁定，请 5 分钟后再试".into());
                    }
                }
            }
        }

        let role = match self.find_user(username) {
            Some(user) => {
                if hash_eq(&user.password_hash, &hash_password(&user.salt, password)) {
                    Some(user.role)
                } else {
                    None
                }
            }
            None => None,
        };

        match role {
            Some(role) => {
                self.login_attempts.lock().remove(username);
                let token = self.create_session(username, role);
                Ok((token, role))
            }
            None => {
                let mut attempts = self.login_attempts.lock();
                let attempt = attempts.entry(username.to_string()).or_insert(LoginAttempt {
                    failures: 0,
                    locked_until: None,
                });
                attempt.failures += 1;
                if attempt.failures >= MAX_LOGIN_FAILURES {
                    attempt.locked_until = Some(Instant::now() + std::time::Duration::from_secs(LOCKOUT_SECS));
                    attempt.failures = 0;
                    tracing::warn!("[账号] 用户名 {username} 连续登录失败，已临时锁定 {LOCKOUT_SECS}s");
                    return Err("失败次数过多，该账号已被临时锁定，请 5 分钟后再试".into());
                }
                Err("用户名或密码错误".into())
            }
        }
    }

    fn create_session(&self, username: &str, role: Role) -> String {
        let token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let info = SessionInfo {
            username: username.to_string(),
            role,
            expires_at: Utc::now() + Duration::hours(SESSION_TTL_HOURS),
        };
        self.sessions.lock().insert(token.clone(), info);
        token
    }

    /// 按 token 取会话用户（滑动续期：剩余不足一半时续满）。
    pub fn user_for_token(&self, token: Option<&str>) -> Option<AuthUser> {
        let token = token?;
        let mut sessions = self.sessions.lock();
        let info = sessions.get_mut(token)?;
        if Utc::now() >= info.expires_at {
            sessions.remove(token);
            return None;
        }
        if info.expires_at - Utc::now() < Duration::hours(SESSION_TTL_HOURS / 2) {
            info.expires_at = Utc::now() + Duration::hours(SESSION_TTL_HOURS);
        }
        Some(AuthUser {
            username: info.username.clone(),
            role: info.role,
        })
    }

    pub fn remove_session(&self, token: Option<&str>) {
        if let Some(token) = token {
            self.sessions.lock().remove(token);
        }
    }

    pub fn active_session_count(&self) -> usize {
        self.sessions.lock().len()
    }

    // ----------------------- 用户管理 -----------------------

    pub fn list_users(&self) -> Vec<Value> {
        self.users
            .read()
            .iter()
            .map(|u| {
                json!({
                    "username": u.username,
                    "role": u.role.as_str(),
                    "created_at": u.created_at,
                })
            })
            .collect()
    }

    pub fn create_user(&self, username: &str, password: &str, role: Role) -> Result<(), String> {
        let username = username.trim();
        if username.len() < 2 || username.len() > 32 {
            return Err("用户名长度需在 2-32 个字符之间".into());
        }
        if !username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || !c.is_ascii())
        {
            return Err("用户名仅支持字母、数字、下划线、中划线或中文".into());
        }
        if password.len() < 6 {
            return Err("密码长度至少 6 位".into());
        }
        let mut users = self.users.write();
        if users.iter().any(|u| u.username == username) {
            return Err(format!("用户 {username} 已存在"));
        }
        let salt = new_salt();
        users.push(UserRecord {
            username: username.to_string(),
            role,
            salt: salt.clone(),
            password_hash: hash_password(&salt, password),
            created_at: Utc::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        });
        drop(users);
        self.save_to_disk();
        tracing::info!("[账号] 新增用户 {username}（角色 {}）", role.as_str());
        Ok(())
    }

    /// 删除用户：不能删除自己，且必须保留至少一个管理员。
    pub fn delete_user(&self, username: &str, actor: &str) -> Result<(), String> {
        if username == actor {
            return Err("不能删除当前登录的账号".into());
        }
        let mut users = self.users.write();
        let Some(pos) = users.iter().position(|u| u.username == username) else {
            return Err(format!("用户 {username} 不存在"));
        };
        let removed_admin = users[pos].role == Role::Admin;
        if removed_admin && users.iter().filter(|u| u.role == Role::Admin).count() <= 1 {
            return Err("系统至少需要保留一个管理员账号".into());
        }
        users.remove(pos);
        drop(users);
        // 吊销该用户全部会话
        self.sessions
            .lock()
            .retain(|_, s| s.username != username);
        self.save_to_disk();
        tracing::info!("[账号] 用户 {username} 已被 {actor} 删除");
        Ok(())
    }

    /// 管理员重置任意用户密码。
    pub fn reset_password(&self, username: &str, new_password: &str) -> Result<(), String> {
        if new_password.len() < 6 {
            return Err("密码长度至少 6 位".into());
        }
        self.set_password_hash(username, new_password)?;
        tracing::info!("[账号] 用户 {username} 的密码已被管理员重置");
        Ok(())
    }

    /// 用户本人修改密码（需校验旧密码）。
    pub fn change_own_password(
        &self,
        username: &str,
        old_password: &str,
        new_password: &str,
    ) -> Result<(), String> {
        let user = self
            .find_user(username)
            .ok_or_else(|| "当前用户不存在".to_string())?;
        if !hash_eq(&user.password_hash, &hash_password(&user.salt, old_password)) {
            return Err("旧密码不正确".into());
        }
        if new_password.len() < 6 {
            return Err("新密码长度至少 6 位".into());
        }
        self.set_password_hash(username, new_password)
    }

    fn set_password_hash(&self, username: &str, new_password: &str) -> Result<(), String> {
        let mut users = self.users.write();
        let Some(user) = users.iter_mut().find(|u| u.username == username) else {
            return Err(format!("用户 {username} 不存在"));
        };
        user.salt = new_salt();
        user.password_hash = hash_password(&user.salt, new_password);
        drop(users);
        // 密码变更后吊销该用户其它会话（保守起见全部吊销，本人需重新登录）
        self.sessions
            .lock()
            .retain(|_, s| s.username != username);
        self.save_to_disk();
        Ok(())
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

// ---------------------------------------------------------------------------
// 路由访问裁决（纯函数，便于单测）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// 无需登录（EA 桥接、静态资源、登录接口）。
    Public,
    /// 需已登录（任意角色）。
    Authed,
    /// 需管理员（所有写操作）。
    AdminOnly,
}

pub fn route_access(path: &str, method: &Method) -> Access {
    // EA 桥接走独立 BridgeToken，不使用 Web 会话
    if path == "/" || path.starts_with("/static/") || path.starts_with("/bridge") {
        return Access::Public;
    }
    if !path.starts_with("/api/") {
        return Access::Public;
    }
    if path == "/api/auth/login" {
        return Access::Public;
    }
    if path == "/api/auth/me" || path == "/api/auth/logout" || path == "/api/auth/password" {
        return Access::Authed;
    }
    if path == "/api/auth/users" || path.starts_with("/api/auth/users/") {
        return Access::AdminOnly;
    }
    match *method {
        Method::GET | Method::HEAD => Access::Authed,
        _ => Access::AdminOnly,
    }
}

// ---------------------------------------------------------------------------
// 中间件
// ---------------------------------------------------------------------------

pub fn extract_cookie_token(headers: &HeaderMap, name: &str) -> Option<String> {
    for value in headers.get_all(header::COOKIE) {
        let Ok(raw) = value.to_str() else { continue };
        for pair in raw.split(';') {
            let mut parts = pair.trim().splitn(2, '=');
            if parts.next() == Some(name) {
                return parts.next().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
            }
        }
    }
    None
}

pub async fn auth_middleware(
    State(service): State<AppState>,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let path = req.uri().path().to_string();
    let access = route_access(&path, req.method());

    if access == Access::Public {
        return next.run(req).await;
    }

    let token = extract_cookie_token(req.headers(), SESSION_COOKIE);
    let user = service.auth.user_for_token(token.as_deref());

    match user {
        None => (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "detail": "未登录或会话已过期" })),
        )
            .into_response(),
        Some(user) => {
            if access == Access::AdminOnly && !user.is_admin() {
                tracing::warn!(
                    "[账号] 只读账号 {} 尝试执行被禁止的操作: {} {}",
                    user.username,
                    req.method(),
                    path
                );
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({ "detail": "只读账号无权执行此操作（需管理员）" })),
                )
                    .into_response();
            }
            req.extensions_mut().insert(user);
            next.run(req).await
        }
    }
}

fn session_cookie(token: &str) -> String {
    format!(
        "{SESSION_COOKIE}={token}; HttpOnly; SameSite=Lax; Path=/; Max-Age={}",
        SESSION_TTL_HOURS * 3600
    )
}

fn clear_session_cookie() -> String {
    format!("{SESSION_COOKIE}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0")
}

// ---------------------------------------------------------------------------
// 登录 / 登出 / 用户管理 Handlers
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

pub async fn handle_login(State(service): State<AppState>, Json(req): Json<LoginRequest>) -> Response {
    match service.auth.attempt_login(&req.username, &req.password) {
        Ok((token, role)) => {
            tracing::info!("[账号] 用户 {} 登录成功（{}）", req.username, role.as_str());
            (
                StatusCode::OK,
                [(header::SET_COOKIE, session_cookie(&token))],
                Json(json!({ "ok": true, "username": req.username.trim(), "role": role.as_str() })),
            )
                .into_response()
        }
        Err(e) => (StatusCode::UNAUTHORIZED, Json(json!({ "detail": e }))).into_response(),
    }
}

pub async fn handle_logout(State(service): State<AppState>, headers: HeaderMap) -> Response {
    let token = extract_cookie_token(&headers, SESSION_COOKIE);
    service.auth.remove_session(token.as_deref());
    (
        StatusCode::OK,
        [(header::SET_COOKIE, clear_session_cookie())],
        Json(json!({ "ok": true })),
    )
        .into_response()
}

pub async fn handle_me(Extension(user): Extension<AuthUser>) -> Response {
    Json(json!({ "username": user.username, "role": user.role.as_str() })).into_response()
}

pub async fn handle_list_users(
    State(service): State<AppState>,
    Extension(_user): Extension<AuthUser>,
) -> Response {
    Json(json!({ "users": service.auth.list_users() })).into_response()
}

#[derive(Debug, Deserialize)]
pub struct CreateUserRequest {
    pub username: String,
    pub password: String,
    #[serde(default = "default_new_user_role")]
    pub role: String,
}
fn default_new_user_role() -> String {
    "readonly".to_string()
}

pub async fn handle_create_user(
    State(service): State<AppState>,
    Extension(user): Extension<AuthUser>,
    Json(req): Json<CreateUserRequest>,
) -> Response {
    let Some(role) = Role::parse(&req.role) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "detail": "角色仅支持 admin 或 readonly" })),
        )
            .into_response();
    };
    match service.auth.create_user(&req.username, &req.password, role) {
        Ok(()) => {
            let _ = &user;
            Json(json!({ "ok": true })).into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "detail": e }))).into_response(),
    }
}

pub async fn handle_delete_user(
    State(service): State<AppState>,
    Extension(user): Extension<AuthUser>,
    AxumPath(username): AxumPath<String>,
) -> Response {
    match service.auth.delete_user(&username, &user.username) {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "detail": e }))).into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct ResetPasswordRequest {
    pub username: String,
    pub new_password: String,
}

pub async fn handle_reset_password(
    State(service): State<AppState>,
    Extension(_user): Extension<AuthUser>,
    Json(req): Json<ResetPasswordRequest>,
) -> Response {
    match service.auth.reset_password(&req.username, &req.new_password) {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "detail": e }))).into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct ChangePasswordRequest {
    pub old_password: String,
    pub new_password: String,
}

pub async fn handle_change_password(
    State(service): State<AppState>,
    Extension(user): Extension<AuthUser>,
    Json(req): Json<ChangePasswordRequest>,
) -> Response {
    match service
        .auth
        .change_own_password(&user.username, &req.old_password, &req.new_password)
    {
        Ok(()) => Json(json!({ "ok": true, "message": "密码已修改，请重新登录" })).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "detail": e }))).into_response(),
    }
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Method;

    fn temp_users_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "pa_users_test_{tag}_{}.json",
            uuid::Uuid::new_v4().simple()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    // ---------- 口令哈希 ----------

    #[test]
    fn test_password_hash_roundtrip() {
        let salt = "abcdef1234567890";
        let h1 = hash_password(salt, "secret123");
        let h2 = hash_password(salt, "secret123");
        assert_eq!(h1, h2, "同一盐与口令必须得到相同哈希");
        assert!(hash_eq(&h1, &hash_password(salt, "secret123")));
        assert!(!hash_eq(&h1, &hash_password(salt, "wrong-pass")));
        assert_ne!(h1, hash_password("other-salt", "secret123"), "不同盐哈希必须不同");
        assert!(!hash_eq("a".repeat(64).as_str(), "b"), "长度不同直接判否");
    }

    // ---------- 路由访问裁决矩阵 ----------

    #[test]
    fn test_route_access_matrix() {
        // 公开：静态资源、EA 桥接、登录
        assert_eq!(route_access("/", &Method::GET), Access::Public);
        assert_eq!(route_access("/static/app.js", &Method::GET), Access::Public);
        assert_eq!(route_access("/bridge/sync", &Method::POST), Access::Public);
        assert_eq!(route_access("/api/auth/login", &Method::POST), Access::Public);

        // 只读可访问：任意 GET 查询
        assert_eq!(route_access("/api/status", &Method::GET), Access::Authed);
        assert_eq!(route_access("/api/candles", &Method::GET), Access::Authed);
        assert_eq!(route_access("/api/account", &Method::GET), Access::Authed);
        assert_eq!(route_access("/api/config", &Method::GET), Access::Authed);
        assert_eq!(route_access("/api/auth/me", &Method::GET), Access::Authed);

        // 写操作一律管理员：只读账号被服务端硬拦截
        assert_eq!(route_access("/api/analyze", &Method::POST), Access::AdminOnly);
        assert_eq!(route_access("/api/automation", &Method::POST), Access::AdminOnly);
        assert_eq!(route_access("/api/config/save_env", &Method::POST), Access::AdminOnly);
        assert_eq!(route_access("/api/trading_system", &Method::POST), Access::AdminOnly);
        assert_eq!(route_access("/api/trade/cancel", &Method::POST), Access::AdminOnly);
        assert_eq!(route_access("/api/trade/cancel_all", &Method::POST), Access::AdminOnly);
        assert_eq!(
            route_access("/api/history/decisions/x", &Method::DELETE),
            Access::AdminOnly
        );
        assert_eq!(route_access("/api/auth/users", &Method::GET), Access::AdminOnly);
        assert_eq!(route_access("/api/auth/users/bob", &Method::DELETE), Access::AdminOnly);
        assert_eq!(route_access("/api/auth/users/password", &Method::POST), Access::AdminOnly);

        // 未注册的 API 路径同样按方法裁决（默认拒绝写）
        assert_eq!(route_access("/api/something_new", &Method::POST), Access::AdminOnly);
        assert_eq!(route_access("/api/something_new", &Method::GET), Access::Authed);
    }

    // ---------- 用户管理 CRUD ----------

    #[test]
    fn test_user_crud_and_protections() {
        let path = temp_users_path("crud");
        let svc = AuthService::with_users_path(path.clone(), false);

        svc.create_user("boss", "admin-pass-1", Role::Admin).unwrap();
        svc.create_user("guest", "view-pass-1", Role::ReadOnly).unwrap();

        // 重复用户名
        assert!(svc.create_user("boss", "whatever-1", Role::Admin).is_err());
        // 弱口令
        assert!(svc.create_user("short", "123", Role::ReadOnly).is_err());

        // 不能删除自己
        assert!(svc.delete_user("boss", "boss").is_err());
        // 不能删除最后一个管理员
        assert!(svc.delete_user("boss", "guest").is_err());

        // 正常删除只读用户并落盘
        svc.delete_user("guest", "boss").unwrap();
        assert!(svc.list_users().iter().all(|u| u["username"] != "guest"));

        // 重启（重新加载文件）后仍在
        let svc2 = AuthService::with_users_path(path.clone(), false);
        assert!(svc2.list_users().iter().any(|u| u["username"] == "boss"));
        let _ = std::fs::remove_file(&path);
    }

    // ---------- 登录 / 会话 / 锁定 ----------

    #[test]
    fn test_login_session_and_lockout() {
        let path = temp_users_path("login");
        let svc = AuthService::with_users_path(path.clone(), false);
        svc.create_user("alice", "good-pass-9", Role::Admin).unwrap();

        // 错误密码
        assert!(svc.attempt_login("alice", "bad-pass").is_err());
        assert!(svc.attempt_login("ghost", "whatever").is_err());

        // 正确密码 → 会话
        let (token, role) = svc.attempt_login("alice", "good-pass-9").unwrap();
        assert_eq!(role, Role::Admin);
        let user = svc.user_for_token(Some(&token)).unwrap();
        assert_eq!(user.username, "alice");
        assert!(user.is_admin());
        assert!(svc.user_for_token(Some("invalid-token")).is_none());
        assert!(svc.user_for_token(None).is_none());

        // 登出后失效
        svc.remove_session(Some(&token));
        assert!(svc.user_for_token(Some(&token)).is_none());

        // 连续失败 5 次 → 锁定，之后即使密码正确也拒绝
        for _ in 0..(MAX_LOGIN_FAILURES - 1) {
            assert!(svc.attempt_login("alice", "bad-pass").is_err());
        }
        let err = svc.attempt_login("alice", "bad-pass").unwrap_err();
        assert!(err.contains("锁定"), "第 5 次失败应触发锁定: {err}");
        assert!(svc.attempt_login("alice", "good-pass-9").is_err());
        let _ = std::fs::remove_file(&path);
    }

    // ---------- 密码修改 ----------

    #[test]
    fn test_password_change_flow() {
        let path = temp_users_path("pwd");
        let svc = AuthService::with_users_path(path.clone(), false);
        svc.create_user("carol", "old-pass-77", Role::ReadOnly).unwrap();

        // 旧密码错误
        assert!(svc
            .change_own_password("carol", "wrong-old", "new-pass-88")
            .is_err());
        // 正确修改
        svc.change_own_password("carol", "old-pass-77", "new-pass-88")
            .unwrap();
        let (token, role) = svc.attempt_login("carol", "new-pass-88").unwrap();
        assert_eq!(role, Role::ReadOnly);
        assert!(svc.attempt_login("carol", "old-pass-77").is_err());
        let _ = svc.remove_session(Some(&token));

        // 管理员重置
        svc.reset_password("carol", "reset-pass-66").unwrap();
        assert!(svc.attempt_login("carol", "reset-pass-66").is_ok());
        let _ = std::fs::remove_file(&path);
    }

    // ---------- Cookie 提取 ----------

    #[test]
    fn test_extract_cookie_token() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            header::HeaderValue::from_static("other=1; pa_session=abc123; x=y"),
        );
        assert_eq!(
            extract_cookie_token(&headers, SESSION_COOKIE).as_deref(),
            Some("abc123")
        );
        assert_eq!(extract_cookie_token(&headers, "missing"), None);
    }
}
