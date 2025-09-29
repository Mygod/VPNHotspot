# WebServerManager 增强实现文档

## 概述

本文档描述了对 `WebServerManager` 和 `OkHttpWebServer` 的增强实现，以解决任务3中指定的资源清理和错误处理问题。

## 实现的功能

### 1. 改进的 `stop()` 方法以确保完整的资源清理

#### WebServerManager.stop()
- 添加了详细的日志记录，跟踪停止过程
- 实现了异常处理，确保即使出现错误也能继续清理
- 添加了短暂等待以确保资源被释放
- 确保服务器引用被正确清除

#### OkHttpWebServer.stop()
- 实现了分阶段的资源清理：
  1. 关闭服务器套接字，停止接受新连接
  2. 取消协程作用域
  3. 优雅关闭线程池，带超时机制
  4. 清理HTTP客户端资源
- 添加了超时机制，避免无限等待
- 实现了强制关闭机制，当优雅关闭失败时使用

### 2. 端口冲突检测和解决逻辑

#### 端口可用性检测
```kotlin
private fun isPortAvailable(port: Int): Boolean {
    return try {
        ServerSocket(port).use { 
            true 
        }
    } catch (e: IOException) {
        false
    }
}
```

#### 端口冲突解决
- 定义了备用端口列表：`[9999, 10000, 10001, 10002, 10003]`
- 实现了智能端口选择逻辑，优先使用配置的端口
- 当首选端口不可用时，自动尝试备用端口

### 3. 备用端口的重试机制

#### 重试逻辑实现
```kotlin
private fun startWithPortRetry(context: Context, preferredPort: Int) {
    val portsToTry = if (preferredPort in FALLBACK_PORTS) {
        listOf(preferredPort) + FALLBACK_PORTS.filter { it != preferredPort }
    } else {
        listOf(preferredPort) + FALLBACK_PORTS
    }
    
    var lastException: Exception? = null
    
    for (port in portsToTry) {
        try {
            // 检查端口可用性
            if (!isPortAvailable(port)) continue
            
            // 尝试启动服务器
            currentServer = OkHttpWebServer(context.applicationContext, port)
            currentServer!!.start()
            
            // 更新配置如果使用了备用端口
            if (port != preferredPort) {
                setPort(port)
            }
            
            return // 成功启动
        } catch (e: Exception) {
            lastException = e
            continue // 尝试下一个端口
        }
    }
    
    // 所有端口都失败，抛出异常
    throw IOException("Failed to start WebServer on any available port", lastException)
}
```

### 4. 全面的异常处理和日志记录

#### 异常处理策略
- **BindException**: 端口被占用时的特殊处理
- **IOException**: 一般I/O错误的处理
- **InterruptedException**: 线程中断的处理
- **通用异常**: 未预期错误的兜底处理

#### 日志记录级别
- **INFO**: 重要的状态变化（启动、停止、端口变更）
- **DEBUG**: 详细的调试信息（端口检查、资源清理步骤）
- **WARN**: 警告信息（端口冲突、清理超时）
- **ERROR**: 错误信息（启动失败、清理失败）

## 新增的功能

### 1. 强制停止方法
```kotlin
fun forceStop() {
    try {
        Timber.w("Force stopping WebServer")
        currentServer?.let { server ->
            try {
                server.stop()
            } catch (e: Exception) {
                Timber.e(e, "Error during force stop")
            }
        }
    } finally {
        currentServer = null
        Timber.i("WebServer force stopped and reference cleared")
    }
}
```

### 2. 状态监控
```kotlin
data class WebServerStatus(
    val isRunning: Boolean,
    val currentPort: Int,
    val configuredPort: Int,
    val lastUsedPort: Int,
    val hasServerInstance: Boolean,
    val error: String? = null
)

fun getStatus(): WebServerStatus
```

### 3. 资源清理方法
```kotlin
fun cleanup() {
    try {
        Timber.i("Cleaning up WebServerManager resources")
        forceStop()
        prefs = null
        lastUsedPort = DEFAULT_PORT
        Timber.i("WebServerManager cleanup completed")
    } catch (e: Exception) {
        Timber.e(e, "Error during WebServerManager cleanup")
    }
}
```

### 4. 增强的连接处理
- 添加了套接字超时设置（30秒）
- 改进了输入/输出流的资源管理
- 区分了不同类型的网络异常
- 确保所有资源在finally块中被正确关闭

## 错误处理改进

### 1. 分层错误处理
- **应用层**: WebServerManager处理高级错误（端口冲突、配置错误）
- **服务层**: OkHttpWebServer处理服务器级错误（连接错误、资源清理）
- **连接层**: handleConnection处理单个连接的错误

### 2. 优雅降级
- 当首选端口不可用时，自动使用备用端口
- 当优雅关闭失败时，使用强制关闭
- 当部分资源清理失败时，继续清理其他资源

### 3. 超时机制
- 线程池关闭超时：5秒优雅关闭 + 2秒强制关闭
- 套接字连接超时：30秒
- 资源清理总超时：通过分阶段实现避免无限等待

## 测试验证

### 1. 单元测试概念
创建了 `test_webserver_manager.py` 脚本来验证：
- 端口可用性检测
- 端口冲突模拟
- 资源清理时间测试

### 2. 集成测试建议
- 测试端口冲突场景下的自动重试
- 测试长时间运行后的资源清理
- 测试应用重启场景
- 测试并发连接处理

## 性能考虑

### 1. 启动性能
- 端口检查使用快速的套接字绑定测试
- 重试机制限制在5个端口内
- 避免了阻塞式的端口扫描

### 2. 停止性能
- 分阶段关闭避免资源泄露
- 超时机制防止无限等待
- 异步清理不阻塞主线程

### 3. 内存管理
- 及时释放套接字资源
- 正确关闭线程池
- 清理协程作用域

## 安全考虑

### 1. 资源泄露防护
- 所有资源都在finally块中清理
- 异常情况下的强制清理机制
- 引用清除防止内存泄露

### 2. 端口安全
- 只使用预定义的端口范围
- 避免随机端口扫描
- 记录端口使用情况

## 向后兼容性

所有现有的公共API保持不变：
- `start(context: Context)`
- `stop()`
- `restart(context: Context)`
- `isRunning(): Boolean`
- `getCurrentPort(): Int`

新增的方法都是可选的，不影响现有代码。

## 总结

本次增强实现了任务3的所有要求：
1. ✅ 改进`stop()`方法以确保完整的资源清理
2. ✅ 添加端口冲突检测和解决逻辑
3. ✅ 实现备用端口的重试机制（9999、10000、10001）
4. ✅ 添加全面的异常处理和日志记录

这些改进显著提高了WebServer的稳定性和可靠性，解决了长时间运行后无法访问的问题，并提供了更好的错误恢复能力。