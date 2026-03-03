// 凭据状态响应
export interface CredentialsStatusResponse {
  total: number
  available: number
  currentId: number
  credentials: CredentialStatusItem[]
}

// 单个凭据状态
export interface CredentialStatusItem {
  id: number
  priority: number
  disabled: boolean
  failureCount: number
  isCurrent: boolean
  expiresAt: string | null
  authMethod: string | null
  hasProfileArn: boolean
  email?: string
  refreshTokenHash?: string
  successCount: number
  lastUsedAt: string | null
  hasProxy: boolean
  proxyUrl?: string
}

// 余额响应
export interface BalanceResponse {
  id: number
  subscriptionTitle: string | null
  currentUsage: number
  usageLimit: number
  remaining: number
  usagePercentage: number
  nextResetAt: number | null
}

// Token 统计响应
export interface TokenStatsResponse {
  snapshotVersion: number
  capturedAt: string
  totalRequests: number
  successfulRequests: number
  failedRequests: number
  totalTokens: number
  cacheTokens: number
  thinkingTokens: number
  rpm: number
  tpm: number
}

// 可用凭据用量汇总响应
export interface CredentialUsageSummaryResponse {
  snapshotVersion: number
  capturedAt: string
  lastRefreshAt?: string
  lastRefreshTrigger?: string
  lastRefreshStatus: string
  lastRefreshError?: string
  availableCredentialCount: number
  queriedCredentialCount: number
  failedCredentialCount: number
  totalUsageLimit: number
  totalCurrentUsage: number
  totalRemaining: number
  remainingPercentage: number
}

// 成功响应
export interface SuccessResponse {
  success: boolean
  message: string
}

// 错误响应
export interface AdminErrorResponse {
  error: {
    type: string
    message: string
  }
}

// 请求类型
export interface SetDisabledRequest {
  disabled: boolean
}

export interface SetPriorityRequest {
  priority: number
}

// 添加凭据请求
export interface AddCredentialRequest {
  refreshToken: string
  authMethod?: 'social' | 'idc'
  clientId?: string
  clientSecret?: string
  priority?: number
  authRegion?: string
  apiRegion?: string
  machineId?: string
  proxyUrl?: string
  proxyUsername?: string
  proxyPassword?: string
}

// 添加凭据响应
export interface AddCredentialResponse {
  success: boolean
  message: string
  credentialId: number
  email?: string
}
