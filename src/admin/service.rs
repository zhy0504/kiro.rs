//! Admin API 业务逻辑服务

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration as StdDuration;

use chrono::Utc;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::token_manager::MultiTokenManager;

use super::error::AdminServiceError;
use super::types::{
    AddCredentialRequest, AddCredentialResponse, BalanceResponse, CredentialStatusItem,
    CredentialUsageSummaryResponse, CredentialsStatusResponse, LoadBalancingModeResponse,
    SetLoadBalancingModeRequest, TokenStatsResponse,
};

/// 余额缓存过期时间（秒），5 分钟
const BALANCE_CACHE_TTL_SECS: i64 = 300;
/// 汇总可用凭据用量时，单个凭据查询超时（秒）
const USAGE_SUMMARY_PER_CREDENTIAL_TIMEOUT_SECS: u64 = 3;
/// 5 分钟：检查统计变化并持久化
const SNAPSHOT_CHECK_INTERVAL_SECS: u64 = 300;
/// 10 分钟：检查 total_tokens 变化以决定是否刷新用量
const USAGE_REFRESH_CHECK_INTERVAL_SECS: u64 = 600;
/// 用量兜底最大陈旧时间（24h）
const USAGE_STALE_FALLBACK_SECS: i64 = 24 * 60 * 60;

const SNAPSHOT_SCHEMA_VERSION: u32 = 1;

/// 最近一次用量刷新状态
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum UsageRefreshStatus {
    Unknown,
    Success,
    Failed,
}

impl UsageRefreshStatus {
    fn as_str(&self) -> &'static str {
        match self {
            UsageRefreshStatus::Unknown => "unknown",
            UsageRefreshStatus::Success => "success",
            UsageRefreshStatus::Failed => "failed",
        }
    }
}

/// 聚合快照（落盘）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AdminStatsSnapshotFile {
    schema_version: u32,
    snapshot_version: u64,
    captured_at: String,
    stats: TokenStatsResponse,
    usage_summary: CredentialUsageSummaryResponse,
    last_usage_refresh_at: Option<String>,
    last_usage_refresh_trigger: Option<String>,
    last_usage_refresh_status: UsageRefreshStatus,
    last_usage_refresh_error: Option<String>,
}

/// 运行时聚合状态
#[derive(Debug, Clone)]
struct AggregatedSnapshotState {
    snapshot_version: u64,
    token_stats: TokenStatsResponse,
    usage_summary: CredentialUsageSummaryResponse,
    last_usage_refresh_at: Option<String>,
    last_usage_refresh_trigger: Option<String>,
    last_usage_refresh_status: UsageRefreshStatus,
    last_usage_refresh_error: Option<String>,
    last_written_stats: Option<(u64, u64, u64, u64)>,
    last_token_check_total_tokens: Option<u64>,
}

impl AggregatedSnapshotState {
    fn new() -> Self {
        Self {
            snapshot_version: 0,
            token_stats: TokenStatsResponse {
                total_requests: 0,
                successful_requests: 0,
                failed_requests: 0,
                total_tokens: 0,
                cache_tokens: 0,
                thinking_tokens: 0,
                rpm: 0,
                tpm: 0,
                snapshot_version: 0,
                captured_at: String::new(),
            },
            usage_summary: CredentialUsageSummaryResponse {
                available_credential_count: 0,
                queried_credential_count: 0,
                failed_credential_count: 0,
                total_usage_limit: 0.0,
                total_current_usage: 0.0,
                total_remaining: 0.0,
                remaining_percentage: 0.0,
                snapshot_version: 0,
                captured_at: String::new(),
                last_refresh_at: None,
                last_refresh_trigger: None,
                last_refresh_status: UsageRefreshStatus::Unknown.as_str().to_string(),
                last_refresh_error: None,
            },
            last_usage_refresh_at: None,
            last_usage_refresh_trigger: None,
            last_usage_refresh_status: UsageRefreshStatus::Unknown,
            last_usage_refresh_error: None,
            last_written_stats: None,
            last_token_check_total_tokens: None,
        }
    }
}

/// 缓存的余额条目（含时间戳）
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedBalance {
    /// 缓存时间（Unix 秒）
    cached_at: f64,
    /// 缓存的余额数据
    data: BalanceResponse,
}

/// Admin 服务
///
/// 封装所有 Admin API 的业务逻辑
pub struct AdminService {
    token_manager: Arc<MultiTokenManager>,
    balance_cache: Mutex<HashMap<u64, CachedBalance>>,
    cache_path: Option<PathBuf>,
    snapshot_path: Option<PathBuf>,
    snapshot_state: Mutex<AggregatedSnapshotState>,
    snapshot_task_started: AtomicBool,
    snapshot_update_lock: tokio::sync::Mutex<()>,
}

impl AdminService {
    pub fn new(token_manager: Arc<MultiTokenManager>) -> Self {
        let cache_path = token_manager
            .cache_dir()
            .map(|d| d.join("kiro_balance_cache.json"));
        let snapshot_path = token_manager
            .cache_dir()
            .map(|d| d.join("kiro_admin_stats_snapshot.json"));

        let balance_cache = Self::load_balance_cache_from(&cache_path);
        let snapshot_state = Self::load_snapshot_state_from(&snapshot_path);

        if snapshot_state.snapshot_version > 0 {
            token_manager.hydrate_runtime_totals(
                snapshot_state.token_stats.total_requests,
                snapshot_state.token_stats.successful_requests,
                snapshot_state.token_stats.failed_requests,
                snapshot_state.token_stats.total_tokens,
                snapshot_state.token_stats.cache_tokens,
                snapshot_state.token_stats.thinking_tokens,
            );
            tracing::info!(
                snapshot_version = snapshot_state.snapshot_version,
                total_requests = snapshot_state.token_stats.total_requests,
                total_tokens = snapshot_state.token_stats.total_tokens,
                "已从快照恢复 Token 统计累计值"
            );
        }

        Self {
            token_manager,
            balance_cache: Mutex::new(balance_cache),
            cache_path,
            snapshot_path,
            snapshot_state: Mutex::new(snapshot_state),
            snapshot_task_started: AtomicBool::new(false),
            snapshot_update_lock: tokio::sync::Mutex::new(()),
        }
    }

    pub fn start_background_tasks(self: &Arc<Self>) {
        if self
            .snapshot_task_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }

        let service = Arc::clone(self);
        tokio::spawn(async move {
            service.run_background_snapshot_tasks().await;
        });
    }

    async fn run_background_snapshot_tasks(self: Arc<Self>) {
        tracing::info!(
            "已启动 Admin 快照后台任务（5 分钟统计检查 + 10 分钟用量刷新检查）"
        );

        // 启动即执行一次：
        // 1) 刷新一次 5 分钟统计判断（确保冷启动后尽快落盘）
        // 2) 检查一次 10 分钟逻辑（用于无历史数据时首次初始化用量）
        self.run_five_minute_stats_check().await;
        self.run_ten_minute_usage_check(true).await;

        let mut stats_interval = tokio::time::interval(StdDuration::from_secs(
            SNAPSHOT_CHECK_INTERVAL_SECS,
        ));
        let mut usage_interval = tokio::time::interval(StdDuration::from_secs(
            USAGE_REFRESH_CHECK_INTERVAL_SECS,
        ));
        stats_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        usage_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // 消费 interval 的首次立即 tick
        stats_interval.tick().await;
        usage_interval.tick().await;

        loop {
            tokio::select! {
                _ = stats_interval.tick() => {
                    self.run_five_minute_stats_check().await;
                }
                _ = usage_interval.tick() => {
                    self.run_ten_minute_usage_check(false).await;
                }
            }
        }
    }

    fn load_snapshot_state_from(snapshot_path: &Option<PathBuf>) -> AggregatedSnapshotState {
        let path = match snapshot_path {
            Some(p) => p,
            None => return AggregatedSnapshotState::new(),
        };

        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return AggregatedSnapshotState::new(),
        };

        let file: AdminStatsSnapshotFile = match serde_json::from_str(&content) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("解析 Admin 快照失败，将忽略: {}", e);
                return AggregatedSnapshotState::new();
            }
        };

        if file.schema_version != SNAPSHOT_SCHEMA_VERSION {
            tracing::warn!(
                expected = SNAPSHOT_SCHEMA_VERSION,
                found = file.schema_version,
                "Admin 快照 schemaVersion 不匹配，将按兼容模式读取"
            );
        }

        let mut state = AggregatedSnapshotState::new();
        state.snapshot_version = file.snapshot_version;

        let mut stats = file.stats;
        if stats.snapshot_version == 0 {
            stats.snapshot_version = file.snapshot_version;
        }
        if stats.captured_at.is_empty() {
            stats.captured_at = file.captured_at.clone();
        }
        state.token_stats = stats;

        let mut usage_summary = file.usage_summary;
        if usage_summary.snapshot_version == 0 {
            usage_summary.snapshot_version = file.snapshot_version;
        }
        if usage_summary.captured_at.is_empty() {
            usage_summary.captured_at = file.captured_at.clone();
        }
        if usage_summary.last_refresh_at.is_none() {
            usage_summary.last_refresh_at = file.last_usage_refresh_at.clone();
        }
        if usage_summary.last_refresh_trigger.is_none() {
            usage_summary.last_refresh_trigger = file.last_usage_refresh_trigger.clone();
        }
        if usage_summary.last_refresh_error.is_none() {
            usage_summary.last_refresh_error = file.last_usage_refresh_error.clone();
        }
        if usage_summary.last_refresh_status == UsageRefreshStatus::Unknown.as_str() {
            usage_summary.last_refresh_status = file.last_usage_refresh_status.as_str().to_string();
        }

        state.last_usage_refresh_at = usage_summary
            .last_refresh_at
            .clone()
            .or(file.last_usage_refresh_at.clone());
        state.last_usage_refresh_trigger = usage_summary
            .last_refresh_trigger
            .clone()
            .or(file.last_usage_refresh_trigger.clone());
        state.last_usage_refresh_status = file.last_usage_refresh_status;
        state.last_usage_refresh_error = usage_summary
            .last_refresh_error
            .clone()
            .or(file.last_usage_refresh_error.clone());
        state.usage_summary = usage_summary;

        state.last_written_stats = Some(Self::stats_key_of(&state.token_stats));
        state.last_token_check_total_tokens = Some(state.token_stats.total_tokens);

        tracing::info!(
            snapshot_version = state.snapshot_version,
            captured_at = %state.token_stats.captured_at,
            "已加载 Admin 快照"
        );

        state
    }

    fn save_snapshot(&self) {
        let path = match &self.snapshot_path {
            Some(p) => p,
            None => return,
        };

        let state = self.snapshot_state.lock().clone();
        let file = AdminStatsSnapshotFile {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            snapshot_version: state.snapshot_version,
            captured_at: state.token_stats.captured_at.clone(),
            stats: state.token_stats,
            usage_summary: state.usage_summary,
            last_usage_refresh_at: state.last_usage_refresh_at,
            last_usage_refresh_trigger: state.last_usage_refresh_trigger,
            last_usage_refresh_status: state.last_usage_refresh_status,
            last_usage_refresh_error: state.last_usage_refresh_error,
        };

        let json = match serde_json::to_string_pretty(&file) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("序列化 Admin 快照失败: {}", e);
                return;
            }
        };

        let tmp_path = path.with_extension("tmp");
        if let Err(e) = std::fs::write(&tmp_path, json) {
            tracing::warn!("写入 Admin 快照临时文件失败: {}", e);
            return;
        }

        if let Err(e) = std::fs::rename(&tmp_path, path) {
            let replaced = if path.exists() {
                std::fs::remove_file(path).is_ok() && std::fs::rename(&tmp_path, path).is_ok()
            } else {
                false
            };

            if !replaced {
                tracing::warn!("原子替换 Admin 快照失败: {}", e);
                let _ = std::fs::remove_file(&tmp_path);
            }
        }
    }

    async fn run_five_minute_stats_check(&self) {
        let _guard = self.snapshot_update_lock.lock().await;

        let runtime = self.token_manager.token_stats_snapshot();
        let current_key = (
            runtime.total_requests,
            runtime.total_tokens,
            runtime.rpm,
            runtime.tpm,
        );

        let mut should_save = false;
        {
            let mut state = self.snapshot_state.lock();
            let changed = state
                .last_written_stats
                .map(|prev| prev != current_key)
                .unwrap_or(true);

            if !changed {
                return;
            }

            state.snapshot_version = state.snapshot_version.saturating_add(1);
            let captured_at = Self::now_utc_rfc3339();
            state.token_stats = TokenStatsResponse {
                total_requests: runtime.total_requests,
                successful_requests: runtime.successful_requests,
                failed_requests: runtime.failed_requests,
                total_tokens: runtime.total_tokens,
                cache_tokens: runtime.cache_tokens,
                thinking_tokens: runtime.thinking_tokens,
                rpm: runtime.rpm,
                tpm: runtime.tpm,
                snapshot_version: state.snapshot_version,
                captured_at: captured_at.clone(),
            };
            state.last_written_stats = Some(current_key);

            // 仅补齐 usage 元信息，不改动已统计值
            state.usage_summary.last_refresh_at = state.last_usage_refresh_at.clone();
            state.usage_summary.last_refresh_trigger = state.last_usage_refresh_trigger.clone();
            state.usage_summary.last_refresh_status =
                state.last_usage_refresh_status.as_str().to_string();
            state.usage_summary.last_refresh_error = state.last_usage_refresh_error.clone();
            if state.usage_summary.captured_at.is_empty() {
                state.usage_summary.captured_at = captured_at;
            }

            should_save = true;
        }

        if should_save {
            self.save_snapshot();
            tracing::info!("5 分钟统计检查检测到变化，已保存 Admin 快照");
        }
    }

    async fn run_ten_minute_usage_check(&self, startup: bool) {
        let current_total_tokens = self.token_manager.token_stats_snapshot().total_tokens;

        let (prev_total_tokens, last_refresh_at) = {
            let state = self.snapshot_state.lock();
            (
                state.last_token_check_total_tokens,
                state.last_usage_refresh_at.clone(),
            )
        };

        let token_changed = prev_total_tokens
            .map(|prev| prev != current_total_tokens)
            .unwrap_or(false);
        let usage_stale = Self::is_usage_stale(last_refresh_at.as_deref());

        let trigger = if token_changed {
            Some("token_changed")
        } else if usage_stale {
            Some("stale_fallback")
        } else if startup && last_refresh_at.is_none() {
            Some("startup")
        } else {
            None
        };

        let refresh_success = if let Some(trigger) = trigger {
            self.refresh_usage_summary_now(trigger.to_string(), true).await
        } else {
            true
        };

        let mut state = self.snapshot_state.lock();
        if state.last_token_check_total_tokens.is_none() {
            state.last_token_check_total_tokens = Some(current_total_tokens);
        } else if token_changed && refresh_success {
            state.last_token_check_total_tokens = Some(current_total_tokens);
        }
    }

    pub async fn refresh_usage_summary_now(&self, trigger: String, bypass_cache: bool) -> bool {
        let _guard = self.snapshot_update_lock.lock().await;

        let latest_stats = self.token_manager.token_stats_snapshot();
        let collected = self
            .collect_credential_usage_summary(bypass_cache)
            .await;

        let fatal_failed = collected.available_credential_count > 0
            && collected.queried_credential_count == 0
            && collected.failed_credential_count > 0;
        let now = Self::now_utc_rfc3339();

        {
            let mut state = self.snapshot_state.lock();
            state.snapshot_version = state.snapshot_version.saturating_add(1);

            state.token_stats = TokenStatsResponse {
                total_requests: latest_stats.total_requests,
                successful_requests: latest_stats.successful_requests,
                failed_requests: latest_stats.failed_requests,
                total_tokens: latest_stats.total_tokens,
                cache_tokens: latest_stats.cache_tokens,
                thinking_tokens: latest_stats.thinking_tokens,
                rpm: latest_stats.rpm,
                tpm: latest_stats.tpm,
                snapshot_version: state.snapshot_version,
                captured_at: now.clone(),
            };
            state.last_written_stats = Some(Self::stats_key_of(&state.token_stats));

            state.last_usage_refresh_at = Some(now.clone());
            state.last_usage_refresh_trigger = Some(trigger.clone());

            if fatal_failed {
                state.last_usage_refresh_status = UsageRefreshStatus::Failed;
                state.last_usage_refresh_error = Some(format!(
                    "可用凭据用量刷新失败：{} 个凭据全部查询失败",
                    collected.failed_credential_count
                ));

                // 失败时保留前次成功的用量值，仅更新刷新状态元信息
                state.usage_summary.snapshot_version = state.snapshot_version;
                state.usage_summary.last_refresh_at = state.last_usage_refresh_at.clone();
                state.usage_summary.last_refresh_trigger = state.last_usage_refresh_trigger.clone();
                state.usage_summary.last_refresh_status =
                    UsageRefreshStatus::Failed.as_str().to_string();
                state.usage_summary.last_refresh_error = state.last_usage_refresh_error.clone();
            } else {
                state.last_usage_refresh_status = UsageRefreshStatus::Success;
                state.last_usage_refresh_error = None;

                state.usage_summary = CredentialUsageSummaryResponse {
                    available_credential_count: collected.available_credential_count,
                    queried_credential_count: collected.queried_credential_count,
                    failed_credential_count: collected.failed_credential_count,
                    total_usage_limit: collected.total_usage_limit,
                    total_current_usage: collected.total_current_usage,
                    total_remaining: collected.total_remaining,
                    remaining_percentage: collected.remaining_percentage,
                    snapshot_version: state.snapshot_version,
                    captured_at: now.clone(),
                    last_refresh_at: state.last_usage_refresh_at.clone(),
                    last_refresh_trigger: state.last_usage_refresh_trigger.clone(),
                    last_refresh_status: UsageRefreshStatus::Success.as_str().to_string(),
                    last_refresh_error: None,
                };
            }
        }

        self.save_snapshot();

        if fatal_failed {
            tracing::warn!(trigger = %trigger, "用量刷新失败，已保留前次统计值");
            false
        } else {
            tracing::info!(trigger = %trigger, "用量刷新成功，已保存 Admin 快照");
            true
        }
    }

    fn now_utc_rfc3339() -> String {
        Utc::now().to_rfc3339()
    }

    fn stats_key_of(stats: &TokenStatsResponse) -> (u64, u64, u64, u64) {
        (stats.total_requests, stats.total_tokens, stats.rpm, stats.tpm)
    }

    fn is_usage_stale(last_refresh_at: Option<&str>) -> bool {
        let Some(ts) = last_refresh_at else {
            return true;
        };

        let parsed = chrono::DateTime::parse_from_rfc3339(ts)
            .map(|dt| dt.with_timezone(&Utc))
            .ok();

        let Some(parsed) = parsed else {
            return true;
        };

        (Utc::now().timestamp() - parsed.timestamp()) >= USAGE_STALE_FALLBACK_SECS
    }

    /// 获取所有凭据状态
    pub fn get_all_credentials(&self) -> CredentialsStatusResponse {
        let snapshot = self.token_manager.snapshot();

        let mut credentials: Vec<CredentialStatusItem> = snapshot
            .entries
            .into_iter()
            .map(|entry| CredentialStatusItem {
                id: entry.id,
                priority: entry.priority,
                disabled: entry.disabled,
                failure_count: entry.failure_count,
                is_current: entry.id == snapshot.current_id,
                expires_at: entry.expires_at,
                auth_method: entry.auth_method,
                has_profile_arn: entry.has_profile_arn,
                refresh_token_hash: entry.refresh_token_hash,
                email: entry.email,
                success_count: entry.success_count,
                last_used_at: entry.last_used_at.clone(),
                has_proxy: entry.has_proxy,
                proxy_url: entry.proxy_url,
            })
            .collect();

        // 按优先级排序（数字越小优先级越高）
        credentials.sort_by_key(|c| c.priority);

        CredentialsStatusResponse {
            total: snapshot.total,
            available: snapshot.available,
            current_id: snapshot.current_id,
            credentials,
        }
    }

    /// 计算所有可用凭据的用量汇总（内部方法，可选择是否绕过缓存）
    async fn collect_credential_usage_summary(
        &self,
        bypass_balance_cache: bool,
    ) -> CredentialUsageSummaryResponse {
        let snapshot = self.token_manager.snapshot();
        let available_ids: Vec<u64> = snapshot
            .entries
            .iter()
            .filter(|cred| !cred.disabled)
            .map(|cred| cred.id)
            .collect();

        let mut queried_credential_count: u64 = 0;
        let mut failed_credential_count: u64 = 0;
        let mut total_usage_limit: f64 = 0.0;
        let mut total_current_usage: f64 = 0.0;
        let mut total_remaining: f64 = 0.0;

        for id in &available_ids {
            let result = if bypass_balance_cache {
                tokio::time::timeout(
                    StdDuration::from_secs(USAGE_SUMMARY_PER_CREDENTIAL_TIMEOUT_SECS),
                    self.fetch_balance(*id),
                )
                .await
            } else {
                tokio::time::timeout(
                    StdDuration::from_secs(USAGE_SUMMARY_PER_CREDENTIAL_TIMEOUT_SECS),
                    self.get_balance(*id),
                )
                .await
            };

            match result {
                Ok(Ok(balance)) => {
                    queried_credential_count += 1;
                    total_usage_limit += balance.usage_limit;
                    total_current_usage += balance.current_usage;
                    total_remaining += balance.remaining;
                }
                Ok(Err(err)) => {
                    failed_credential_count += 1;
                    tracing::warn!("聚合可用凭据用量时查询失败 #{}: {}", id, err);
                }
                Err(_) => {
                    failed_credential_count += 1;
                    tracing::warn!(
                        "聚合可用凭据用量时查询超时 #{}（>{}s）",
                        id,
                        USAGE_SUMMARY_PER_CREDENTIAL_TIMEOUT_SECS
                    );
                }
            }
        }

        let remaining_percentage = if total_usage_limit > 0.0 {
            ((total_remaining / total_usage_limit) * 100.0).clamp(0.0, 100.0)
        } else {
            0.0
        };

        CredentialUsageSummaryResponse {
            available_credential_count: available_ids.len() as u64,
            queried_credential_count,
            failed_credential_count,
            total_usage_limit,
            total_current_usage,
            total_remaining,
            remaining_percentage,
            snapshot_version: 0,
            captured_at: String::new(),
            last_refresh_at: None,
            last_refresh_trigger: None,
            last_refresh_status: UsageRefreshStatus::Unknown.as_str().to_string(),
            last_refresh_error: None,
        }
    }

    /// 获取全局请求/Token 统计（优先返回快照，确保跨重启连续）
    pub fn get_token_stats(&self) -> TokenStatsResponse {
        let state = self.snapshot_state.lock();
        state.token_stats.clone()
    }

    /// 获取所有可用凭据的用量汇总（返回最近快照）
    pub async fn get_credential_usage_summary(&self) -> CredentialUsageSummaryResponse {
        let state = self.snapshot_state.lock();
        state.usage_summary.clone()
    }

    /// 设置凭据禁用状态
    pub async fn set_disabled(&self, id: u64, disabled: bool) -> Result<(), AdminServiceError> {
        // 先获取当前凭据 ID，用于判断是否需要切换
        let snapshot = self.token_manager.snapshot();
        let current_id = snapshot.current_id;

        self.token_manager
            .set_disabled(id, disabled)
            .map_err(|e| self.classify_error(e, id))?;

        // 只有禁用的是当前凭据时才尝试切换到下一个
        if disabled && id == current_id {
            let _ = self.token_manager.switch_to_next();
        }

        let _ = self
            .refresh_usage_summary_now("credential_changed".to_string(), true)
            .await;
        Ok(())
    }

    /// 设置凭据优先级
    pub async fn set_priority(&self, id: u64, priority: u32) -> Result<(), AdminServiceError> {
        self.token_manager
            .set_priority(id, priority)
            .map_err(|e| self.classify_error(e, id))?;

        let _ = self
            .refresh_usage_summary_now("credential_changed".to_string(), true)
            .await;

        Ok(())
    }

    /// 重置失败计数并重新启用
    pub async fn reset_and_enable(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .reset_and_enable(id)
            .map_err(|e| self.classify_error(e, id))?;

        let _ = self
            .refresh_usage_summary_now("credential_changed".to_string(), true)
            .await;

        Ok(())
    }

    /// 获取凭据余额（带缓存）
    pub async fn get_balance(&self, id: u64) -> Result<BalanceResponse, AdminServiceError> {
        // 先查缓存
        {
            let cache = self.balance_cache.lock();
            if let Some(cached) = cache.get(&id) {
                let now = Utc::now().timestamp() as f64;
                if (now - cached.cached_at) < BALANCE_CACHE_TTL_SECS as f64 {
                    tracing::debug!("凭据 #{} 余额命中缓存", id);
                    return Ok(cached.data.clone());
                }
            }
        }

        // 缓存未命中或已过期，从上游获取
        let balance = self.fetch_balance(id).await?;

        // 更新缓存
        {
            let mut cache = self.balance_cache.lock();
            cache.insert(
                id,
                CachedBalance {
                    cached_at: Utc::now().timestamp() as f64,
                    data: balance.clone(),
                },
            );
        }
        self.save_balance_cache();

        Ok(balance)
    }

    /// 从上游获取余额（无缓存）
    async fn fetch_balance(&self, id: u64) -> Result<BalanceResponse, AdminServiceError> {
        let usage = self
            .token_manager
            .get_usage_limits_for(id)
            .await
            .map_err(|e| self.classify_balance_error(e, id))?;

        let current_usage = usage.current_usage();
        let usage_limit = usage.usage_limit();
        let remaining = (usage_limit - current_usage).max(0.0);
        let usage_percentage = if usage_limit > 0.0 {
            (current_usage / usage_limit * 100.0).min(100.0)
        } else {
            0.0
        };

        Ok(BalanceResponse {
            id,
            subscription_title: usage.subscription_title().map(|s| s.to_string()),
            current_usage,
            usage_limit,
            remaining,
            usage_percentage,
            next_reset_at: usage.next_date_reset,
        })
    }

    /// 添加新凭据
    pub async fn add_credential(
        &self,
        req: AddCredentialRequest,
    ) -> Result<AddCredentialResponse, AdminServiceError> {
        // 构建凭据对象
        let email = req.email.clone();
        let new_cred = KiroCredentials {
            id: None,
            access_token: None,
            refresh_token: Some(req.refresh_token),
            profile_arn: None,
            expires_at: None,
            auth_method: Some(req.auth_method),
            client_id: req.client_id,
            client_secret: req.client_secret,
            priority: req.priority,
            region: req.region,
            auth_region: req.auth_region,
            api_region: req.api_region,
            machine_id: req.machine_id,
            email: req.email,
            subscription_title: None, // 将在首次获取使用额度时自动更新
            proxy_url: req.proxy_url,
            proxy_username: req.proxy_username,
            proxy_password: req.proxy_password,
            disabled: false, // 新添加的凭据默认启用
        };

        // 调用 token_manager 添加凭据
        let credential_id = self
            .token_manager
            .add_credential(new_cred)
            .await
            .map_err(|e| self.classify_add_error(e))?;

        // 主动获取订阅等级，避免首次请求时 Free 账号绕过 Opus 模型过滤
        if let Err(e) = self.token_manager.get_usage_limits_for(credential_id).await {
            tracing::warn!("添加凭据后获取订阅等级失败（不影响凭据添加）: {}", e);
        }

        let _ = self
            .refresh_usage_summary_now("credential_changed".to_string(), true)
            .await;

        Ok(AddCredentialResponse {
            success: true,
            message: format!("凭据添加成功，ID: {}", credential_id),
            credential_id,
            email,
        })
    }

    /// 删除凭据
    pub async fn delete_credential(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .delete_credential(id)
            .map_err(|e| self.classify_delete_error(e, id))?;

        // 清理已删除凭据的余额缓存
        {
            let mut cache = self.balance_cache.lock();
            cache.remove(&id);
        }
        self.save_balance_cache();

        let _ = self
            .refresh_usage_summary_now("credential_changed".to_string(), true)
            .await;

        Ok(())
    }

    /// 获取负载均衡模式
    pub fn get_load_balancing_mode(&self) -> LoadBalancingModeResponse {
        LoadBalancingModeResponse {
            mode: self.token_manager.get_load_balancing_mode(),
        }
    }

    /// 设置负载均衡模式
    pub fn set_load_balancing_mode(
        &self,
        req: SetLoadBalancingModeRequest,
    ) -> Result<LoadBalancingModeResponse, AdminServiceError> {
        // 验证模式值
        if req.mode != "priority" && req.mode != "balanced" {
            return Err(AdminServiceError::InvalidCredential(
                "mode 必须是 'priority' 或 'balanced'".to_string(),
            ));
        }

        self.token_manager
            .set_load_balancing_mode(req.mode.clone())
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        Ok(LoadBalancingModeResponse { mode: req.mode })
    }

    // ============ 余额缓存持久化 ============

    fn load_balance_cache_from(cache_path: &Option<PathBuf>) -> HashMap<u64, CachedBalance> {
        let path = match cache_path {
            Some(p) => p,
            None => return HashMap::new(),
        };

        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return HashMap::new(),
        };

        // 文件中使用字符串 key 以兼容 JSON 格式
        let map: HashMap<String, CachedBalance> = match serde_json::from_str(&content) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("解析余额缓存失败，将忽略: {}", e);
                return HashMap::new();
            }
        };

        let now = Utc::now().timestamp() as f64;
        map.into_iter()
            .filter_map(|(k, v)| {
                let id = k.parse::<u64>().ok()?;
                // 丢弃超过 TTL 的条目
                if (now - v.cached_at) < BALANCE_CACHE_TTL_SECS as f64 {
                    Some((id, v))
                } else {
                    None
                }
            })
            .collect()
    }

    fn save_balance_cache(&self) {
        let path = match &self.cache_path {
            Some(p) => p,
            None => return,
        };

        // 持有锁期间完成序列化和写入，防止并发损坏
        let cache = self.balance_cache.lock();
        let map: HashMap<String, &CachedBalance> =
            cache.iter().map(|(k, v)| (k.to_string(), v)).collect();

        match serde_json::to_string_pretty(&map) {
            Ok(json) => {
                if let Err(e) = std::fs::write(path, json) {
                    tracing::warn!("保存余额缓存失败: {}", e);
                }
            }
            Err(e) => tracing::warn!("序列化余额缓存失败: {}", e),
        }
    }

    // ============ 错误分类 ============

    /// 分类简单操作错误（set_disabled, set_priority, reset_and_enable）
    fn classify_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        let msg = e.to_string();
        if msg.contains("不存在") {
            AdminServiceError::NotFound { id }
        } else {
            AdminServiceError::InternalError(msg)
        }
    }

    /// 分类余额查询错误（可能涉及上游 API 调用）
    fn classify_balance_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        let msg = e.to_string();

        // 1. 凭据不存在
        if msg.contains("不存在") {
            return AdminServiceError::NotFound { id };
        }

        // 2. 上游服务错误特征：HTTP 响应错误或网络错误
        let is_upstream_error =
            // HTTP 响应错误（来自 refresh_*_token 的错误消息）
            msg.contains("凭证已过期或无效") ||
            msg.contains("权限不足") ||
            msg.contains("已被限流") ||
            msg.contains("服务器错误") ||
            msg.contains("Token 刷新失败") ||
            msg.contains("暂时不可用") ||
            // 网络错误（reqwest 错误）
            msg.contains("error trying to connect") ||
            msg.contains("connection") ||
            msg.contains("timeout") ||
            msg.contains("timed out");

        if is_upstream_error {
            AdminServiceError::UpstreamError(msg)
        } else {
            // 3. 默认归类为内部错误（本地验证失败、配置错误等）
            // 包括：缺少 refreshToken、refreshToken 已被截断、无法生成 machineId 等
            AdminServiceError::InternalError(msg)
        }
    }

    /// 分类添加凭据错误
    fn classify_add_error(&self, e: anyhow::Error) -> AdminServiceError {
        let msg = e.to_string();

        // 凭据验证失败（refreshToken 无效、格式错误等）
        let is_invalid_credential = msg.contains("缺少 refreshToken")
            || msg.contains("refreshToken 为空")
            || msg.contains("refreshToken 已被截断")
            || msg.contains("凭据已存在")
            || msg.contains("refreshToken 重复")
            || msg.contains("凭证已过期或无效")
            || msg.contains("权限不足")
            || msg.contains("已被限流");

        if is_invalid_credential {
            AdminServiceError::InvalidCredential(msg)
        } else if msg.contains("error trying to connect")
            || msg.contains("connection")
            || msg.contains("timeout")
        {
            AdminServiceError::UpstreamError(msg)
        } else {
            AdminServiceError::InternalError(msg)
        }
    }

    /// 分类删除凭据错误
    fn classify_delete_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        let msg = e.to_string();
        if msg.contains("不存在") {
            AdminServiceError::NotFound { id }
        } else if msg.contains("只能删除已禁用的凭据") || msg.contains("请先禁用凭据") {
            AdminServiceError::InvalidCredential(msg)
        } else {
            AdminServiceError::InternalError(msg)
        }
    }
}
