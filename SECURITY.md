# 安全策略

## 支持的版本

| 版本 | 支持 |
|------|------|
| 最新 `main` 分支 | 是 |
| 旧 tag / 分支 | 仅作参考，不保证修复 |

## 报告漏洞或密钥泄露

**请勿在公开 Issue 中粘贴 API Key、加密后的 `api_key_encrypted`、完整 `settings.json` 或含个人账号信息的分析记录。**

请通过以下方式私下联系维护者：

- GitHub：**Security Advisories**（仓库 → Security → Report a vulnerability），或
- GitHub Issues（**不含任何密钥或账号信息**）

报告中请尽量包含：

- 问题类型（误提交密钥、本地文件权限、依赖漏洞等）
- 影响范围与复现步骤
- 是否已在公开仓库历史中暴露密钥（如是，请说明大致时间以便协助轮换）

## 用户自查清单

1. 确认 `config/settings.json` 未被 `git add`（应被 `.gitignore` 忽略）。
2. 确认 `.gitignore` 覆盖了 `.env`、`config/settings.json`、`config/users.json`、`records/`；提交前先看一遍 `git status` 与 `git diff --cached`。
3. 若 Key 曾进入 Git 历史：在服务商处轮换 Key，并清理 Git 历史或作废旧仓库镜像。
4. 开源 fork 时删除 `records/`、`logs/`、`experience/` 中的私人数据后再推送。

## 免责声明

本软件为交易分析辅助工具，不提供托管服务；安全配置（API、MT5 账户）由使用者自行负责。
